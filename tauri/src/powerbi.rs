//! Power BI export: a small star schema of CSV files (UTF-8 with BOM, `.` decimals).
//!   usage.csv     one row per day x tool x model x project (the fact table)
//!   models.csv    model_key -> tool, model, price family, current list rates
//!   projects.csv  project_key -> name, path
//!   calendar.csv  every date between the first and last usage day
//! Relationships: usage[model_key] -> models, usage[project_key] -> projects, usage[date] -> calendar.
use crate::events::{self, Event};
use crate::pricing::{antigravity_pricing, claude_pricing, codex_pricing, resolve};
use chrono::{Datelike, Local, NaiveDate};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// CSV field: quoted when needed; text that a spreadsheet would run as a formula gets a `'`.
fn cell(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s }
}

#[derive(Default)]
struct Fact {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    calls: u64,
    cost: f64,
}

fn model_key(e: &Event) -> String {
    format!("{}|{}", e.tool, e.model)
}

/// Builds the four CSV files' contents from parsed events.
pub fn tables(events: &[Event]) -> [(&'static str, String); 4] {
    let mut facts: BTreeMap<(String, String, String), Fact> = BTreeMap::new();
    let mut models: BTreeMap<String, (&str, String)> = BTreeMap::new();
    let mut projects: BTreeMap<String, String> = BTreeMap::new();
    for e in events {
        let f = facts.entry((e.day.clone(), model_key(e), e.project.clone())).or_default();
        f.input += e.input;
        f.output += e.output;
        f.cache_read += e.cache_read;
        f.cache_creation += e.cache_creation;
        f.calls += 1;
        f.cost += e.cost;
        models.entry(model_key(e)).or_insert((e.tool, e.model.clone()));
        projects.entry(e.project.clone()).or_insert_with(|| e.project_name.clone());
    }

    let mut usage = String::from("date,model_key,project_key,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,calls,cost_usd\n");
    for ((day, mk, pk), f) in &facts {
        let _ = writeln!(usage, "{},{},{},{},{},{},{},{},{:.6}", day, cell(mk), cell(pk),
                         f.input, f.output, f.cache_read, f.cache_creation, f.calls, f.cost);
    }

    let today = Local::now().format("%Y-%m-%d").to_string();
    let mut mcsv = String::from("model_key,tool,model,price_family,input_usd_per_mtok,output_usd_per_mtok,priced\n");
    for (mk, (tool, model)) in &models {
        let rate = match *tool {
            "claude_api" => claude_pricing(model, 0),
            "codex" => codex_pricing(model),
            _ => antigravity_pricing(model, &today),
        };
        let (i, o, priced) = resolve(model, &today, &rate);
        let _ = writeln!(mcsv, "{},{},{},{},{},{},{}", cell(mk), tool, cell(model), cell(&rate.0), i, o, priced);
    }

    let mut pcsv = String::from("project_key,project_name\n");
    for (pk, name) in &projects {
        let _ = writeln!(pcsv, "{},{}", cell(pk), cell(name));
    }

    let mut cal = String::from("date,year,quarter,month,month_label,iso_week,weekday,weekday_name\n");
    let parse = |d: &String| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok();
    let days: Vec<NaiveDate> = facts.keys().filter_map(|k| parse(&k.0)).collect();
    if let (Some(first), Some(last)) = (days.iter().min(), days.iter().max()) {
        let mut d = *first;
        while d <= *last {
            let _ = writeln!(cal, "{},{},{},{},{},{},{},{}", d.format("%Y-%m-%d"), d.year(), (d.month() - 1) / 3 + 1,
                             d.month(), d.format("%Y-%m"), d.iso_week().week(), d.weekday().number_from_monday(), d.format("%A"));
            d = d.succ_opt().unwrap_or(*last + chrono::Duration::days(1));
        }
    }
    [("usage.csv", usage), ("models.csv", mcsv), ("projects.csv", pcsv), ("calendar.csv", cal)]
}

/// Power BI project (PBIP: TMDL model + 3-page report) that loads the CSVs above.
/// `{{EXPORT_FOLDER}}` becomes the export folder; open `TokenScope.pbip` and refresh.
const PBIP: &[(&str, &str)] = &[
    ("TokenScope.pbip", include_str!("../powerbi/TokenScope.pbip")),
    ("TokenScope.Report/definition.pbir", include_str!("../powerbi/TokenScope.Report/definition.pbir")),
    ("TokenScope.Report/report.json", include_str!("../powerbi/TokenScope.Report/report.json")),
    ("TokenScope.SemanticModel/definition.pbism", include_str!("../powerbi/TokenScope.SemanticModel/definition.pbism")),
    ("TokenScope.SemanticModel/definition/database.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/database.tmdl")),
    ("TokenScope.SemanticModel/definition/expressions.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/expressions.tmdl")),
    ("TokenScope.SemanticModel/definition/model.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/model.tmdl")),
    ("TokenScope.SemanticModel/definition/relationships.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/relationships.tmdl")),
    ("TokenScope.SemanticModel/definition/tables/dates.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/tables/dates.tmdl")),
    ("TokenScope.SemanticModel/definition/tables/models.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/tables/models.tmdl")),
    ("TokenScope.SemanticModel/definition/tables/projects.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/tables/projects.tmdl")),
    ("TokenScope.SemanticModel/definition/tables/usage.tmdl", include_str!("../powerbi/TokenScope.SemanticModel/definition/tables/usage.tmdl")),
];

/// Writes the whole history into `out` (the folder the user picked) and returns it. The
/// report is written only once per folder, so edits made to it in Power BI survive later exports.
pub fn export(out: &Path) -> std::io::Result<PathBuf> {
    let out = out.to_path_buf();
    fs::create_dir_all(&out)?;
    for (name, body) in tables(&events::load_events(100_000, None)) {
        fs::write(out.join(name), format!("\u{feff}{body}"))?;
    }
    if !out.join("TokenScope.pbip").exists() {
        let folder = out.display().to_string().replace('"', "\"\""); // inside an M string literal
        for (rel, body) in PBIP {
            let path = out.join(rel);
            fs::create_dir_all(path.parent().unwrap_or(&out))?;
            fs::write(path, body.replace("{{EXPORT_FOLDER}}", &folder))?;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(day: &str, model: &str, project: &str, cost: f64) -> Event {
        Event {
            tool: "codex", model: model.into(), project: project.into(), project_name: "=calc".into(),
            input: 10, output: 5, cache_read: 1, cache_creation: 0, cost, ts: None, day: day.into(),
            dk: None, priced: true,
        }
    }

    #[test]
    fn star_schema() {
        let [usage, models, projects, cal] = tables(&[
            ev("2026-10-30", "gpt-6-luna", "d:/a,b", 0.5),
            ev("2026-10-30", "gpt-6-luna", "d:/a,b", 0.25),
            ev("2026-11-01", "gpt-6-sol", "d:/c", 1.0),
        ]);
        let rows: Vec<&str> = usage.1.lines().collect();
        assert_eq!(rows.len(), 3); // header + 2 grouped rows
        assert_eq!(rows[1], "2026-10-30,codex|gpt-6-luna,\"d:/a,b\",20,10,2,0,2,0.750000");
        assert!(models.1.contains("codex|gpt-6-luna,codex,gpt-6-luna,GPT-6 Luna,0.1,0.5,true"));
        assert!(projects.1.contains("d:/c,'=calc")); // formula guard
        assert_eq!(cal.1.lines().count(), 1 + 3); // 30, 31 Oct, 1 Nov
        assert!(cal.1.contains("2026-11-01,2026,4,11,2026-11,44,7,Sunday"));
    }

    #[test]
    fn pbip_written_once_with_folder() {
        let dir = std::env::temp_dir().join(format!("ts-pbip-{}", std::process::id()));
        let out = dir.join("TokenScope-PowerBI");
        fs::create_dir_all(&out).unwrap();
        for (rel, body) in PBIP {
            let p = out.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body.replace("{{EXPORT_FOLDER}}", &out.display().to_string())).unwrap();
        }
        let expr = fs::read_to_string(out.join("TokenScope.SemanticModel/definition/expressions.tmdl")).unwrap();
        assert!(expr.contains(&format!("\"{}\"", out.display())) && !expr.contains("{{"));
        assert!(PBIP.iter().all(|(_, b)| !b.is_empty()));
        let _ = fs::remove_dir_all(dir);
    }
}
