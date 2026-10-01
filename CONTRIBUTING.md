# Contributing to PRCtrl

Thank you for your interest in contributing to PRCtrl! This document provides guidelines and instructions for contributing.

## Getting Started

### Prerequisites

- Latest stable Rust ([Install via rustup](https://rustup.rs/)); a minimum supported Rust version is not currently declared
- GitHub personal access token (for local testing)
- macOS (for notification features)

### Development Setup

1. **Fork and clone the repository**
   ```bash
   git clone https://github.com/YOUR_USERNAME/prctrl
   cd prctrl
   ```

2. **Build the project**
   ```bash
   cargo build
   ```

3. **Run tests**
   ```bash
   cargo test
   ```

4. **Run with clippy (linting)**
   ```bash
   cargo clippy -- -D warnings
   ```

## Making Changes

### Before You Start

- Check existing issues and PRs to avoid duplication
- For significant changes, open an issue first to discuss the approach

### Development Workflow

1. **Create a feature branch**
   ```bash
   git checkout -b feature/your-feature-name
   # or
   git checkout -b fix/your-bug-fix
   ```

2. **Make your changes**
   - Write clean, readable code
   - Add tests for new functionality
   - Update documentation as needed

3. **Run quality checks**
   ```bash
   # Build
   cargo build
   
   # Lint
   cargo clippy -- -D warnings
   
   # Test
   cargo test
   
   # Build docs
   cd docs && mdbook build
   ```

4. **Commit your changes**
   ```bash
   git add .
   git commit -m "feat: add new feature"
   ```

   Use conventional commit prefixes:
   - `feat:` New feature
   - `fix:` Bug fix
   - `docs:` Documentation changes
   - `refactor:` Code refactoring
   - `improvement:` General improvements
   - `test:` Adding/updating tests

5. **Push and create a PR**
   ```bash
   git push origin feature/your-feature-name
   ```

## Code Guidelines

### Rust Style

- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use meaningful variable and function names
- Add doc comments for public APIs
- Handle errors with anyhow/thiserror

### Error Handling

- Use descriptive error messages
- Provide actionable hints in errors
- Don't expose internal implementation details

### CLI Design

- Keep help text concise and helpful
- Use sensible defaults (no required flags to start)
- Support `--json` for scripting
- Provide both interactive and non-interactive modes

## Documentation

### Updating Documentation

If your change affects user-facing behavior:

1. Update the relevant command docs in `docs/src/commands/`
2. Update `docs/src/SUMMARY.md` if adding new commands
3. Rebuild docs: `cd docs && mdbook build`
4. Update README.md if applicable

### Doc Style

- Use clear, concise language
- Include practical examples
- Show common use cases

## Testing

### Running Tests

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_name

# Run with output
cargo test -- --nocapture
```

### Writing Tests

- Unit tests go in the same file as the code
- Integration tests go in `tests/` directory
- Use descriptive test names: `test_should_do_x_when_y`

## Project Structure

```
prctrl/
├── src/
│   ├── main.rs      # Entry point, command handling
│   ├── cli.rs       # Clap CLI definitions
│   ├── config.rs    # Configuration loading
│   ├── github.rs    # GitHub API integration
│   ├── dispatcher.rs # Claude integration
│   ├── logger.rs    # Output formatting
│   ├── notifications.rs # macOS notifications
│   ├── terminal.rs  # Terminal utilities
│   └── writer.rs    # File output
├── docs/            # mdBook documentation
├── tests/           # Integration tests
└── Cargo.toml       # Project manifest
```

## Debugging

### Logging

Use `println!` for debug output during development:
```rust
println!("Debug: variable = {:?}", value);
```

### Testing Specific Commands

```bash
# Test config command
cargo run -- config init

# Test list command
cargo run -- list

# Test with debug output
RUST_LOG=debug cargo run -- list
```

## Questions?

- Open an issue for bugs or feature requests
- Check existing issues before duplicating

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
