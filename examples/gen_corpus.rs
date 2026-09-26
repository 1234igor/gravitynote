//! Write a synthetic note corpus to a file, for testing the app at scale.
//!
//! ```bash
//! cargo run --release --example gen_corpus -- /tmp/big.md          # 20 years
//! cargo run --release --example gen_corpus -- /tmp/small.md 500    # 500 notes
//! ```
//!
//! Point GravityNote at the result by copying it over
//! `~/Library/Application Support/gravitynote-gpui/note.md` — back up first.

use std::time::Instant;

use gravitynote::corpus::{self, CorpusSpec};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "corpus.md".to_string());
    let spec = match args.next() {
        Some(n) => CorpusSpec::with_notes(n.parse().expect("note count must be a number")),
        None => CorpusSpec::twenty_years(),
    };

    let started = Instant::now();
    let text = corpus::generate(spec);
    let generated = started.elapsed();
    let stats = corpus::stats(&text);

    std::fs::write(&path, &text).expect("write corpus");

    println!(
        "{path}: {} notes, {} lines, {:.2} MB, {} chars (generated in {:?})",
        stats.notes,
        stats.lines,
        stats.bytes as f64 / 1_048_576.,
        stats.chars,
        generated,
    );
}
