//! Generate a deterministic, *realistic* corpus on disk for the CLI harness.
//!
//! Unlike a flat uniform tree, this one is `git init`-ed with a real
//! `.gitignore`, a heavy-tailed file-size distribution, hidden files, a binary
//! file, and a large (>4 MiB) file — so the walk cache's `.gitignore`-parsing
//! advantage and grep's binary/oversize fallbacks are actually exercised.
//!
//! Usage: gen-corpus <dir> [num_files] [dirs] [lines_per_file]

use std::path::Path;
use std::process::Command;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .expect("usage: gen-corpus <dir> [num_files] [dirs] [lines]");
    let num_files: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(2000);
    let dirs: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(32);
    let lines: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(200);

    let root = Path::new(&dir);
    std::fs::create_dir_all(root).unwrap();

    // Heavy-tailed sizes: most files small, some large, one huge.
    let paths = hearth_bench::gen_corpus_skewed(root, num_files, dirs, lines);

    // A single big file (>4 MiB) to exercise grep's search_path fallback.
    std::fs::write(root.join("BIG.rs"), hearth_bench::file_text(99, 120_000)).unwrap();

    // A binary file (has NUL bytes) so binary detection is exercised.
    std::fs::write(root.join("blob.bin"), vec![0u8, 1, 2, 3, 0, 255, 42, 0]).unwrap();

    // A hidden file and a `.gitignore` with real ignore content + an ignored
    // subtree that both rg and a cold hearth must parse rules to skip.
    std::fs::write(root.join(".env.local"), "SECRET=TODO_MATCH\n").unwrap();
    std::fs::write(
        root.join(".gitignore"),
        "target/\nnode_modules/\n*.log\ndist/\n.env.local\n",
    )
    .unwrap();
    for sub in ["target", "node_modules", "dist"] {
        let d = root.join(sub);
        std::fs::create_dir_all(&d).unwrap();
        for i in 0..300 {
            std::fs::write(
                d.join(format!("junk{i:04}.rs")),
                hearth_bench::file_text(i as u64, 80),
            )
            .unwrap();
        }
    }
    for i in 0..200 {
        std::fs::write(root.join(format!("build{i:03}.log")), "TODO_MATCH noise\n").unwrap();
    }

    // Make it a real git repo so .gitignore is honored exactly as in a real project.
    let _ = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(root)
        .status();

    let bytes: u64 = paths
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    eprintln!("generated {num_files} tracked files ({dirs} dirs) + BIG.rs + binary + hidden");
    eprintln!("plus ~900 git-ignored files (target/node_modules/dist) + 200 *.log to be skipped");
    eprintln!("tracked corpus bytes: {bytes}");
}
