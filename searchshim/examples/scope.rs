//! Crawl-scope probe: is "does Explorer use the index here?" the gate that decides
//! whether our search-engine replacement is consulted at all?
//!
//! Explorer answers a search in one of two ways: through the index (which goes to
//! the OLE DB provider we replace) or by walking the filesystem itself (which does
//! not). If the switch is simply whether the folder is in the crawl scope, then
//! making Explorer *believe* every folder is indexed is enough to route every
//! search through our engine: no second implementation needed.
//!
//! Usage:
//!   scope status <path>     is this path in the crawl scope?
//!   scope add <path>        add it (small folders only; this makes the indexer crawl)
//!   scope remove <path>     take it back out

use std::env;

use windows::core::{Result, HSTRING, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Search::{CSearchManager, ISearchManager};

fn url_for(path: &str) -> String {
    // The crawl scope speaks URLs: file:///C:\dir
    format!("file:///{path}")
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: scope <status|add|remove> <path>");
        return Ok(());
    }
    let (cmd, path) = (args[1].as_str(), args[2].as_str());
    let url = url_for(path);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mgr: ISearchManager = CoCreateInstance(&CSearchManager, None, CLSCTX_ALL)?;
        let catalog = mgr.GetCatalog(&HSTRING::from("SystemIndex"))?;
        let scope = catalog.GetCrawlScopeManager()?;
        let u = HSTRING::from(url.as_str());

        match cmd {
            "status" => {
                let included = scope.IncludedInCrawlScope(PCWSTR(u.as_ptr()))?;
                println!(
                    "{path}\n  url      : {url}\n  in scope : {}",
                    included.as_bool()
                );
            }
            "add" => {
                scope.AddUserScopeRule(PCWSTR(u.as_ptr()), true, false, 0)?;
                scope.SaveAll()?;
                println!("added to crawl scope: {url}");
            }
            "remove" => {
                scope.RemoveScopeRule(PCWSTR(u.as_ptr()))?;
                scope.SaveAll()?;
                println!("removed from crawl scope: {url}");
            }
            // How many items the catalog holds. Watching this across a scope
            // change answers the question that decides whether this route is
            // usable at all: does marking a place "indexed" make Windows go and
            // index it, or only change the answer Explorer gets?
            "count" => {
                let n = catalog.NumberOfItems()?;
                println!("indexed items: {n}");
            }
            // Every rule the catalog holds, which is where the answer to "what does
            // Windows itself keep out of a search?" lives. Hard-coding a list of noisy
            // folders would be one machine's opinion; the catalog's own exclusions are
            // the same list Explorer is working from, on any machine and in any locale.
            "rules" => {
                let mut n = 0;
                let rules = scope.EnumerateScopeRules()?;
                loop {
                    let mut got = [None];
                    let mut fetched = 0u32;
                    if rules.Next(&mut got, &mut fetched).is_err() || fetched == 0 {
                        break;
                    }
                    let Some(rule) = got[0].take() else { break };
                    let url = rule.PatternOrURL()?;
                    let included = rule.IsIncluded()?.as_bool();
                    let default = rule.IsDefault()?.as_bool();
                    let kind = if included { "include" } else { "EXCLUDE" };
                    let origin = if default { "default" } else { "user   " };
                    println!("{kind}  {origin}  {}", url.to_string()?);
                    n += 1;
                }
                println!("({n} rules)");
            }
            other => eprintln!("unknown command: {other}"),
        }
    }
    Ok(())
}
