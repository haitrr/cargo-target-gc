//! The report. Every byte figure is reclaim, not apparent size.

use std::path::Path;

use crate::collect::Category;
use crate::fsutil::human;

fn tier_name(t: char) -> &'static str {
    match t {
        'A' => "free",
        'B' => "recompile unmarked configs",
        _ => "first-edit penalty",
    }
}

pub fn sort_key(c: &Category) -> (i64, i64) {
    ((c.cost * 1000.0) as i64, -(c.acct.reclaim() as i64))
}

pub fn report(cats: &[Category], apply: bool, budget_note: Option<&str>) {
    if cats.is_empty() {
        println!("{}", budget_note.unwrap_or("nothing to collect — target/ is already tidy"));
        return;
    }
    let width = cats.iter().map(|c| c.key.len()).max().unwrap_or(8);
    println!(
        "{:<width$}  {:>9}  {:>9}  items  cost",
        "category",
        "reclaim",
        "shared",
        width = width
    );
    println!("{}", "-".repeat(width + 46));

    let mut order: Vec<&Category> = cats.iter().collect();
    order.sort_by_key(|c| sort_key(c));

    let mut total = 0u64;
    let mut shared = 0u64;
    for c in &order {
        total += c.acct.reclaim();
        shared += c.acct.shared();
        println!(
            "{:<width$}  {:>9}  {:>9}  {:>5}  {}",
            c.key,
            human(c.acct.reclaim()),
            human(c.acct.shared()),
            c.paths.len(),
            tier_name(c.tier),
            width = width
        );
        for line in wrap(&c.blurb, 72) {
            println!("{:<width$}  {}", "", line, width = width);
        }
    }
    println!("{}", "-".repeat(width + 46));
    println!(
        "{:<width$}  {:>9}  {:>9}",
        "total",
        human(total),
        human(shared),
        width = width
    );

    if shared > 0 {
        let n: usize = cats.iter().map(|c| c.acct.shared_files()).sum();
        println!(
            "\n{n} file(s) still carry a hardlink this run does NOT delete, so their\n\
             {} does not come back yet. Three things do that: rustc hardlinks unchanged\n\
             object files into the next incremental session, cargo uplifts a binary into\n\
             the profile root, and some setups seed deps/ across checkouts — for that last\n\
             one, run this in the sibling checkouts too.",
            human(shared)
        );
    }
    if let Some(note) = budget_note {
        println!("\n{note}");
    }
    if !apply {
        println!("\ndry run — nothing deleted. Re-run with --apply.");
    }
}

pub fn json_summary(cats: &[Category], target_dir: &Path, apply: bool) -> serde_json::Value {
    let mut order: Vec<&Category> = cats.iter().collect();
    order.sort_by_key(|c| sort_key(c));
    serde_json::json!({
        "target": target_dir,
        "applied": apply,
        "reclaimable_bytes": cats.iter().map(|c| c.acct.reclaim()).sum::<u64>(),
        "shared_bytes": cats.iter().map(|c| c.acct.shared()).sum::<u64>(),
        "categories": order.iter().map(|c| serde_json::json!({
            "key": c.key,
            "tier": c.tier.to_string(),
            "items": c.paths.len(),
            "reclaim_bytes": c.acct.reclaim(),
            "shared_bytes": c.acct.shared(),
        })).collect::<Vec<_>>(),
    })
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}
