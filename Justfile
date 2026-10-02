run *args:
  cargo run --release -- {{args}}

test:
  cargo nextest run

fmt:
  cargo fmt

clippy:
  cargo clippy --all-targets -- -D warnings

audit:
  cargo audit
