# Contributing

Thanks for your interest in contributing to **bluez-message**! 🎉

## Branching model

This project uses a simple two-branch model:

- **`main`** — stable, released code. Please don't push to it directly; changes land via pull requests.
- **`develop`** — integration branch for ongoing work. **Target your pull requests here.**

Maintainers periodically merge `develop` into `main` for releases.

## How to contribute

1. **Fork** this repository and clone your fork.
2. Create a branch from **`develop`**:
   ```bash
   git checkout develop
   git checkout -b my-change
   ```
3. Make your changes with clear, focused commits.
4. Push to your fork and open a **Pull Request targeting `develop`**.

## Guidelines

- Keep each pull request focused on a single change.
- Explain *what* changes and *why* in the PR description.
- Match the existing code style and conventions.
- Update the README / docs when behavior or configuration changes.
- Test your change before submitting, and describe how you tested it.

## Reporting issues

Open an issue describing: what you expected, what actually happened, and the
steps to reproduce it. Include versions / environment when relevant.

## License

By contributing, you agree that your contributions are licensed under the same
license as this project (see the repository's license file).
