//! What Windows itself keeps out of a search, asked of Windows rather than decided here.
//!
//! Everyfind indexes the whole volume, which is the point of it and also the reason an Explorer
//! search filled up with things Explorer has never shown anyone: shortcut stubs under
//! `AppData\Roaming\Microsoft\Windows\Recent`, copies under `AppData\Local\Temp`, the recycle
//! bin. Windows' own search does not show them because its crawl scope excludes them, and that
//! scope is readable, so the list of what to leave out does not have to be written down here,
//! machine by machine and locale by locale. It is whatever this machine's Windows says it is,
//! including the folders the user themselves excluded in Indexing Options.
//!
//! Measured on this machine: 137 rules, of which Windows' own are
//!
//! ```text
//! EXCLUDE  *\$RECYCLE.BIN\   *\DfsrPrivate\   *\System Volume Information\
//! EXCLUDE  C:\windows\   C:\Windows.*\   C:\windows\*\temp\   C:\windows\CSC\
//! EXCLUDE  C:\ProgramData\        include  C:\ProgramData\Microsoft\Windows\Start Menu\
//! EXCLUDE  C:\Users\*\AppData\    include  C:\Users\   (and two package carve-outs)
//! ```
//!
//! Read from the registry rather than through `ISearchCrawlScopeManager`, because this runs on
//! an Explorer thread inside explorer.exe: a registry read cannot fail in a way that takes the
//! shell with it, and a `CoCreateInstance` on somebody else's thread might.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

/// The merged rule set: defaults, group policy and the user's own additions, already
/// reconciled by Windows. `DefaultRules` next to it holds only the shipped ones.
const RULES_KEY: &str =
    r"SOFTWARE\Microsoft\Windows Search\CrawlScopeManager\Windows\SystemIndex\WorkingSetRules";

/// One crawl-scope rule, reduced to what a filter needs.
#[derive(Clone)]
struct Rule {
    /// Lower-cased path pattern: the `file:///` scheme and the volume-GUID token stripped, the
    /// trailing separator kept because a rule always names a subtree (`c:\users\*\appdata\`).
    pattern: String,
    include: bool,
    /// Windows' own rule rather than one the user added. Only these say what Windows *would*
    /// have indexed; a user's own rule says what they wanted kept out of it, which is a
    /// different question and answered separately.
    windows_own: bool,
}

/// This machine's crawl scope, as far as a path filter cares.
#[derive(Clone, Default)]
pub struct Rules(Vec<Rule>);

impl Rules {
    /// Would Windows have answered a search of this folder out of its index?
    ///
    /// This is the question that decides whether its exclusions apply at all. Explorer answers
    /// a search one of two ways: from the index, which honours every exclusion, or by walking
    /// the folder itself, which honours none of them. A search of `C:\` or of `C:\Windows` is
    /// the second kind (nothing there is indexed), so hiding anything from it would be
    /// narrower than the Explorer we are standing in for, not closer to it.
    pub fn indexes(&self, path_lc: &str) -> bool {
        self.most_specific(path_lc, true) == Some(true)
    }

    /// Does Windows keep this path out of search results?
    pub fn hides(&self, path_lc: &str) -> bool {
        self.most_specific(path_lc, false) == Some(false)
    }

    /// Whether the most specific rule covering `path` includes or excludes it, or `None` when
    /// no rule covers it at all.
    ///
    /// Longest pattern wins, which is how a carve-out beats the exclusion it sits inside:
    /// `...\ProgramData\Microsoft\Windows\Start Menu\` is longer than `...\ProgramData\`, so the
    /// Start Menu stays searchable while the rest of ProgramData does not.
    fn most_specific(&self, path_lc: &str, windows_own_only: bool) -> Option<bool> {
        self.0
            .iter()
            .filter(|r| !windows_own_only || r.windows_own)
            .filter(|r| under(&r.pattern, path_lc))
            .max_by_key(|r| r.pattern.len())
            .map(|r| r.include)
    }

    /// How many rules were read, for the log line that says what a search was filtered against.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Does `path` lie under `pattern`, the folder itself included?
///
/// `*` stands for any run of characters within one path component, which is all three shapes
/// Windows uses: a whole component (`c:\users\*\appdata\`), the drive in front of a well-known
/// name (`*\$recycle.bin\`) and part of a name (`c:\windows.*\`). Anchoring it to a single
/// component is what keeps `c:\users\*\appdata\` from swallowing an `AppData` five levels down.
fn under(pattern: &str, path_lc: &str) -> bool {
    // Every pattern ends in a separator, so giving the path one too lets a single comparison
    // say both "is this folder" and "is inside this folder".
    let mut haystack = String::with_capacity(path_lc.len() + 1);
    haystack.push_str(path_lc);
    if !haystack.ends_with('\\') {
        haystack.push('\\');
    }

    let mut parts = pattern.split('*');
    let Some(head) = parts.next() else {
        return false;
    };
    if !haystack.starts_with(head) {
        return false;
    }
    let mut at = head.len();
    for literal in parts {
        if literal.is_empty() {
            continue;
        }
        let Some(off) = haystack[at..].find(literal) else {
            return false;
        };
        // The run the `*` stood for has to be one component, so it may not contain a separator.
        if haystack[at..at + off].contains('\\') {
            return false;
        }
        at += off + literal.len();
    }
    true
}

/// Read the crawl scope. Cheap enough to do per search on paper (137 keys), but the caller
/// caches it anyway: the rules change when somebody opens Indexing Options, not between
/// keystrokes.
pub fn load() -> Rules {
    let mut rules: Vec<Rule> = Vec::new();
    unsafe {
        let key = wide(RULES_KEY);
        let mut hkey: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            KEY_READ,
            &mut hkey as *mut HKEY,
        ) != ERROR_SUCCESS
        {
            return Rules(rules);
        }
        let mut i = 0u32;
        loop {
            // Rule keys are decimal numbers, so the buffer is generous by a wide margin.
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let rc = RegEnumKeyExW(
                hkey,
                i,
                name.as_mut_ptr(),
                &mut len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if rc != ERROR_SUCCESS {
                break;
            }
            i += 1;
            if let Some(rule) = read_rule(hkey, name.as_ptr()) {
                rules.push(rule);
            }
        }
        RegCloseKey(hkey);
    }
    Rules(rules)
}

unsafe fn read_rule(parent: HKEY, sub: *const u16) -> Option<Rule> {
    let pattern = pattern_from_url(&reg_sz(parent, sub, "URL")?)?;
    Some(Rule {
        pattern,
        include: reg_dword(parent, sub, "Include")? == 1,
        windows_own: reg_dword(parent, sub, "Default").unwrap_or(0) == 1,
    })
}

/// `file:///C:\[05816791-...-b272e0028dd9]\Users\*\AppData\` -> `c:\users\*\appdata\`.
///
/// The bracketed token is the volume's GUID, which a rule carries so it survives the drive
/// being relettered. Paths we match against never have one, so it comes out along with the
/// separator behind it. Anything that is not a `file:` URL (the two `winrt://` rules that
/// scope the Windows apps) has no path to match and is dropped.
fn pattern_from_url(url: &str) -> Option<String> {
    let mut s = url.strip_prefix("file:///")?.to_lowercase();
    if let Some(open) = s.find('[') {
        if let Some(close) = s[open..].find(']') {
            let mut end = open + close + 1;
            if s[end..].starts_with('\\') {
                end += 1;
            }
            s.replace_range(open..end, "");
        }
    }
    if !s.ends_with('\\') {
        s.push('\\');
    }
    Some(s)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn reg_sz(parent: HKEY, sub: *const u16, value: &str) -> Option<String> {
    let val = wide(value);
    let mut buf = [0u16; 1024];
    let mut cb = (buf.len() * 2) as u32;
    let rc = RegGetValueW(
        parent,
        sub,
        val.as_ptr(),
        RRF_RT_REG_SZ,
        std::ptr::null_mut(),
        buf.as_mut_ptr() as *mut c_void,
        &mut cb,
    );
    if rc != ERROR_SUCCESS {
        return None;
    }
    // Same padding caveat as the shim's own `reg_str`: the reported size can exceed the
    // string, so the string ends at the first NUL and nowhere else.
    let units = &buf[..(cb as usize / 2).min(buf.len())];
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

unsafe fn reg_dword(parent: HKEY, sub: *const u16, value: &str) -> Option<u32> {
    let val = wide(value);
    let mut out: u32 = 0;
    let mut cb = 4u32;
    let rc = RegGetValueW(
        parent,
        sub,
        val.as_ptr(),
        RRF_RT_REG_DWORD,
        std::ptr::null_mut(),
        &mut out as *mut u32 as *mut c_void,
        &mut cb,
    );
    (rc == ERROR_SUCCESS).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::{pattern_from_url, under, Rule, Rules};

    /// The rules this machine actually reported, so the tests are about Windows' scope and not
    /// about an imagined one.
    fn measured() -> Rules {
        let rules = [
            (r"*\$recycle.bin\", false, true),
            (r"*\system volume information\", false, true),
            (r"c:\windows\", false, true),
            (r"c:\windows.*\", false, true),
            (r"c:\windows\*\temp\", false, true),
            (r"c:\programdata\", false, true),
            (r"c:\programdata\microsoft\windows\start menu\", true, true),
            (r"c:\users\", true, true),
            (r"c:\users\*\appdata\", false, true),
            (r"c:\users\*\appdata\local\temp\", false, true),
            // One the user added themselves in Indexing Options.
            (r"c:\users\me\dev\oss-fixes\zed\", false, false),
        ];
        Rules(
            rules
                .into_iter()
                .map(|(pattern, include, windows_own)| Rule {
                    pattern: pattern.to_string(),
                    include,
                    windows_own,
                })
                .collect(),
        )
    }

    #[test]
    fn a_wildcard_stands_for_one_component() {
        assert!(under(
            r"c:\users\*\appdata\",
            r"c:\users\me\appdata\roaming\x.lnk"
        ));
        assert!(
            under(r"c:\users\*\appdata\", r"c:\users\me\appdata"),
            "the folder itself"
        );
        assert!(
            !under(r"c:\users\*\appdata\", r"c:\users\me\dev\appdata\x"),
            "two components deep is a different folder"
        );
        assert!(
            under(r"*\$recycle.bin\", r"c:\$recycle.bin\s-1-5-21\x"),
            "the drive"
        );
        assert!(
            under(r"c:\windows.*\", r"c:\windows.old\x"),
            "part of a name"
        );
        assert!(
            !under(r"c:\windows.*\", r"c:\windows\x"),
            "and only that name"
        );
        assert!(!under(r"c:\users\", r"c:\usersdata\x"), "not a bare prefix");
    }

    #[test]
    fn the_longest_rule_wins() {
        let r = measured();
        assert!(r.hides(r"c:\programdata\ssh\x"), "ProgramData is excluded");
        assert!(
            !r.hides(r"c:\programdata\microsoft\windows\start menu\x.lnk"),
            "but the Start Menu is carved back in by a longer rule"
        );
        assert!(r.hides(r"c:\users\me\appdata\roaming\microsoft\windows\recent\a.lnk"));
        assert!(!r.hides(r"c:\users\me\downloads\a.mp3"));
    }

    /// The user's own exclusions count as hidden (they said so in Indexing Options) but not
    /// as Windows' idea of what it indexes, which is what decides whether to filter at all.
    #[test]
    fn a_user_rule_hides_without_speaking_for_windows() {
        let r = measured();
        assert!(r.hides(r"c:\users\me\dev\oss-fixes\zed\readme.md"));
        assert!(r.indexes(r"c:\users\me\dev\oss-fixes\zed\readme.md"));
    }

    /// Where Windows would have used the index, and so where its exclusions mean anything.
    #[test]
    fn only_an_indexed_folder_is_filtered_like_one() {
        let r = measured();
        assert!(r.indexes(r"c:\users\me"), "the profile is indexed");
        assert!(!r.indexes(r"c:"), "a drive root is not");
        assert!(!r.indexes(r"c:\windows\system32"), "nor is Windows");
        assert!(
            !r.indexes(r"c:\users\me\appdata\roaming"),
            "nor is a folder that is itself excluded: searching inside it hides nothing"
        );
    }

    #[test]
    fn a_url_loses_its_scheme_and_its_volume_guid() {
        assert_eq!(
            pattern_from_url(r"file:///C:\[05816791-492a-4e1b-b04a-b272e0028dd9]\Users\*\AppData\")
                .as_deref(),
            Some(r"c:\users\*\appdata\")
        );
        assert_eq!(
            pattern_from_url(r"file:///*\$RECYCLE.BIN\").as_deref(),
            Some(r"*\$recycle.bin\")
        );
        assert_eq!(
            pattern_from_url(r"file:///C:\eftest\order").as_deref(),
            Some(r"c:\eftest\order\"),
            "a rule always names a subtree, separator or not"
        );
        assert_eq!(
            pattern_from_url("winrt://{S-1-5-21-1351287425}/"),
            None,
            "an app scope has no path to match"
        );
    }
}
