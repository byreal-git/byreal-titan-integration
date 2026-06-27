//! Byreal Titan integration scorecard.
//!
//! This is an always-on, no-RPC structural report. To see the report:
//!
//! ```bash
//! make scorecard
//! cargo test --release --test scorecard -- --nocapture
//! ```

use std::fs;
use std::path::{Path, PathBuf};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(manifest().join(rel)).unwrap_or_default()
}

fn files_under(rel: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&manifest().join(rel), &mut out);
    out
}

fn fill_in_files() -> Vec<(String, usize)> {
    let root = manifest();
    let mut paths: Vec<PathBuf> = ["src", "tests", "program/programs"]
        .iter()
        .flat_map(|&r| files_under(r))
        .collect();
    paths.push(root.join("program/Anchor.toml"));

    let mut out: Vec<(String, usize)> = paths
        .into_iter()
        .filter(|p| !p.ends_with("scorecard.rs"))
        .filter_map(|p| {
            let n = fs::read_to_string(&p).ok()?.matches("FILL_IN:").count();
            (n > 0).then(|| {
                let rel = p
                    .strip_prefix(&root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned();
                (rel, n)
            })
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

const LAYERS: [(&str, &str); 4] = [
    ("Creation parser", "parse_pool_creations() + fixture tests"),
    ("Quote layer", "ByrealClmmVenue implements quote() + price"),
    ("Program layer", "Byreal CPI module + on-chain Venue enum"),
    ("Route builder", "protocol_to_venue mapping + route Venue enum"),
];

fn render_subheader(title: &str) -> String {
    let width = 62usize;
    let prefix = format!("  -- {title} ");
    let dashes = "-".repeat(width.saturating_sub(prefix.len()));
    format!("{prefix}{dashes}\n")
}

fn render_layers(done: [bool; 4]) -> String {
    let mut s = String::from("  Byreal CLMM\n\n");
    s.push_str(&render_subheader("Layers"));
    s.push_str("  Status  Layer            Detail\n");
    s.push_str(
        "  ------  ---------------  ------------------------------------------------------------\n",
    );
    for (i, (layer, desc)) in LAYERS.iter().enumerate() {
        s.push_str(&format!(
            "  {:<6}  {:<15}  {}\n",
            if done[i] { "[x]" } else { "[ ]" },
            layer,
            desc
        ));
    }
    s
}

fn render_fill_in(fill_in: &[(String, usize)]) -> String {
    let fill_in_total: usize = fill_in.iter().map(|(_, n)| n).sum();
    let mut s = String::new();
    s.push('\n');
    s.push_str(&render_subheader("Remaining FILL_IN markers"));
    s.push_str(&format!(
        "  Total: {fill_in_total} marker(s) across {} file(s)\n",
        fill_in.len()
    ));
    s.push_str("  Count  File\n");
    s.push_str("  -----  ------------------------------------------------------------\n");
    for (path, n) in fill_in {
        s.push_str(&format!("  {n:<5}  {path}\n"));
    }
    s
}

fn render_simulation() -> String {
    format!(
        "\n{}  Status    Detail\n  --------  ------------------------------------------------------------\n  {status:<8}  {detail}\n",
        render_subheader("Simulation"),
        status = "SKIPPED",
        detail = "SDK-level simulation is skipped; program route tests use isolated LiteSVM"
    )
}

fn render_summary(done: [bool; 4]) -> String {
    let count = done.iter().filter(|d| **d).count();
    let detail = if done.iter().all(|d| *d) {
        "all wired; LiteSVM route execution is isolated to the program test crate"
    } else {
        "replace the [ ] items above"
    };

    format!(
        "\n{}  Target      Status             Detail\n  ----------  -----------------  ------------------------------------------------------------\n  Byreal      {count}/4 layers wired  {detail}\n",
        render_subheader("Summary")
    )
}

#[test]
fn integration_scorecard() {
    const PROGRAM_SRC: &str = "program/programs/byreal-titan-venue-program/src";

    for f in [
        "src/byreal_clmm/mod.rs",
        "src/byreal_clmm/core.rs",
        "src/swap_route/mod.rs",
        "src/trading_venue/protocol.rs",
        "tests/byreal_clmm_creation.rs",
        "tests/byreal_clmm.rs",
        &format!("{PROGRAM_SRC}/state.rs"),
        &format!("{PROGRAM_SRC}/instructions/venues/byreal_clmm.rs"),
    ] {
        assert!(
            !read(f).is_empty(),
            "integration layer missing or empty: {f}"
        );
    }

    let byreal = read("src/byreal_clmm/mod.rs");
    let byreal_core = read("src/byreal_clmm/core.rs");
    let swap_route = read("src/swap_route/mod.rs");
    let protocol = read("src/trading_venue/protocol.rs");
    let state = read(&format!("{PROGRAM_SRC}/state.rs"));
    let byreal_cpi = read(&format!("{PROGRAM_SRC}/instructions/venues/byreal_clmm.rs"));
    let creation_test = read("tests/byreal_clmm_creation.rs");

    let done = [
        byreal.contains("fn parse_pool_creations")
            && creation_test.contains("parses_byreal_create_pool")
            && creation_test.contains("parses_byreal_create_pool_decay_fee"),
        byreal.contains("impl TradingVenue for ByrealClmmVenue")
            && byreal_core.contains("fn quote(&self"),
        state.contains("ByrealClmm")
            && byreal_cpi.contains("SWAP_V3_DYN_DISCRIMINATOR")
            && !byreal_cpi.contains("11111111111111111111111111111111"),
        protocol.contains("PoolProtocol::ByrealClmm")
            && swap_route.contains("Venue::ByrealClmm"),
    ];

    let fill_in = fill_in_files();

    let mut report = String::new();
    report.push_str("\n================ Byreal Titan integration scorecard =========\n\n");
    report.push_str(&render_layers(done));
    report.push_str(&render_fill_in(&fill_in));
    report.push_str(&render_simulation());
    report.push_str(&render_summary(done));
    report.push_str("=============================================================\n");
    println!("{report}");

    assert!(
        done.iter().all(|d| *d),
        "Byreal integration has incomplete layers",
    );
}
