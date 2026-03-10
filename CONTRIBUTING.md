# Contributing

Contributions are welcome! Bug reports, feature requests, and pull requests are all appreciated.

## Getting started

```bash
cargo build
cargo test
cargo clippy
cargo fmt --check
```

## Adding a new adapter

1. Create `src/adapters/<name>/mod.rs` and `parse.rs`
2. Implement `CliAdapter` for your struct
3. Add the variant to `CliName` in `src/types.rs`
4. Wire it into `get_adapter()` in `src/adapters/mod.rs`
5. Add discovery paths in `src/discovery.rs`
6. Include tests for argument building and output parsing

## Pull requests

- Keep changes focused — one feature or fix per PR
- Add tests for new functionality
- Run `cargo clippy` and `cargo fmt` before submitting

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
