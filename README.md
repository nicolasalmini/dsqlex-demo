# dsqlex-demo

Interactive demo for [dsqlex](https://github.com/nicolasalmini/dsqlex-rs), a SQL-style expression language for rules over structured data.

Two pages, served locally:

- **Pipeline** (`/`): streams a generated 250k-row CSV through a DSQLEX rule in a loop and writes the results to disk. Edit and apply the rule while it runs; each output row is stamped with the rule version that produced it, so you can watch a hot swap happen without stopping the stream.
- **Playground** (`/playground`): evaluates an expression against a JSON context on every keystroke. Nested objects become dot paths (`config.pricing.margin`) and numbers keep full decimal precision.

## Run

Requires Rust 1.88+.

```sh
cargo run --release
```

Open <http://127.0.0.1:3000>. The server only listens on localhost.

On first start it generates `data/sample-250k.csv`. Pipeline output goes to `data/output-<pid>-<n>/`, in two alternating slot files capped at 50k rows each, so disk usage stays bounded.

## Example rule

```sql
CASE
    WHEN country = 'Brazil' THEN amount * 2
    ELSE amount
END
```

Both pages include templates, from a plain field reference up to a nine-branch `CASE` with nested arithmetic.

## Test

```sh
cargo test
node --test tests/*.cjs
```
