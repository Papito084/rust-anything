//! End-to-end test for `card::report` on a real bridged, recorded, and built N-API crate.

use std::path::{Path, PathBuf};

use rust_anything::bridge::{self, Kind};
use rust_anything::card;
use rust_anything::enable;
use rust_anything::harness::{self, RecordOptions};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rust-anything-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn report_on_a_built_crate_writes_markdown_and_svg_and_reflects_enable() {
    let tmp = TempDir::new("card-e2e");
    let root = tmp.path();
    write_file(
        &root.join("package.json"),
        "{\"name\":\"card-e2e\",\"version\":\"1.0.0\"}\n",
    );
    write_file(
        &root.join("src/math.ts"),
        "export function add(a: number, b: number): number { return a + b; }\n\
         export function mul(a: number, b: number): number { return a * b; }\n",
    );
    write_file(
        &root.join("src/index.ts"),
        "import { add, mul } from \"./math\";\n\
         export function run() { return add(2, 3) + mul(2, 3); }\n",
    );

    let crate_dir = root.join("rust/math");
    bridge::bridge(&bridge::Options {
        root,
        unit: "src/math.ts",
        kind: Some(Kind::Napi),
        out: Some(crate_dir.clone()),
        name: None,
        force: false,
        keep: Vec::new(),
        lang: None,
        batch: Vec::new(),
        as_json: Vec::new(),
        allocator: Default::default(),
    })
    .unwrap();

    let recorded = harness::record(&RecordOptions {
        crate_dir: &crate_dir,
        count: 10,
        inputs: None,
        module: None,
        resolve_from: None,
        node_args: &[],
    })
    .unwrap();
    assert!(recorded.cases > 0, "{recorded:?}");

    // Port `add`, and drop `mul` from `src/port.rs`, then refresh so `mul` is missing in Rust and contributes to `fallback_cases`.
    let port_path = crate_dir.join("src/port.rs");
    let port = std::fs::read_to_string(&port_path).unwrap();
    let ported = port.replacen(
        "unimplemented!(\"port `add` from src/math.ts\")",
        "Ok(a + b)",
        1,
    );
    assert_ne!(ported, port);
    let (port_without_mul, _) = ported
        .split_once("/// Port of `mul`")
        .expect("expected mul in port.rs");
    std::fs::write(&port_path, port_without_mul.trim_end()).unwrap();

    bridge::refresh(&crate_dir).unwrap();

    let out_dir = root.join("custom-report");
    let card_before = card::report(&card::Options {
        crate_dir: &crate_dir,
        out: Some(&out_dir),
        min_ms: 5,
        node_args: &[],
        resolve_from: None,
    })
    .unwrap();
    assert_eq!(card_before.markdown, out_dir.join("REPORT.md"));
    assert_eq!(card_before.svg, out_dir.join("card.svg"));
    assert!(card_before.markdown.is_file());
    assert!(card_before.svg.is_file());
    assert!(card_before.matched > 0);
    assert!(card_before.matched < card_before.cases);
    assert_eq!(
        card_before.fallback_cases,
        card_before.cases - card_before.matched
    );
    assert_eq!(card_before.enabled, Some(false));

    let md_before = std::fs::read_to_string(&card_before.markdown).unwrap();
    assert!(
        md_before.contains("| Unit | `src/math.ts` (TypeScript/JavaScript) |"),
        "{md_before}"
    );
    assert!(
        md_before.contains(&format!(
            "| Recorded cases matched | {} / {} |",
            card_before.matched, card_before.cases
        )),
        "{md_before}"
    );
    assert!(
        md_before.contains("| Speedup, call included |"),
        "{md_before}"
    );
    assert!(
        md_before.contains("| Fallback to the original |"),
        "{md_before}"
    );
    assert!(md_before.contains("| Switched on | off |"), "{md_before}");
    assert!(md_before.contains("| `add` |"), "{md_before}");

    let svg = std::fs::read_to_string(&card_before.svg).unwrap();
    let trimmed = svg.trim();
    assert!(trimmed.starts_with("<svg"), "{trimmed}");
    assert!(trimmed.ends_with("</svg>"), "{trimmed}");
    assert!(
        trimmed.contains("xmlns=\"http://www.w3.org/2000/svg\""),
        "{trimmed}"
    );

    enable::enable(&crate_dir).unwrap();

    let card_after = card::report(&card::Options {
        crate_dir: &crate_dir,
        out: Some(&out_dir),
        min_ms: 5,
        node_args: &[],
        resolve_from: None,
    })
    .unwrap();
    assert_eq!(card_after.enabled, Some(true));
    let md_after = std::fs::read_to_string(&card_after.markdown).unwrap();
    assert!(md_after.contains("| Switched on | on |"), "{md_after}");
}
