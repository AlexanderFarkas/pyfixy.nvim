# pyfixy.nvim

A tiny Neovim + Rust LSP sidecar for pytest fixtures.

`pyfixy.nvim` runs next to `ty` and provides pytest fixture-only LSP features:

- completion for fixture names in test function parameters
- go to definition from fixture parameter to fixture declaration
- references from fixture definitions or fixture parameters to fixture usages

Early prototype, but usable for static pytest fixture completion/definition.

## Install with lazy.nvim

```lua
{
  "AlexanderFarkas/pyfixy.nvim",
  ft = "python",
  build = "./scripts/install.sh",
  config = function()
    require("pyfixy").setup()
  end,
}
```

## Current scope

Supported so far:

- `@pytest.fixture`
- `@pytest.fixture(name = "custom_name")`
- `@fixture`
- fixtures in the same test file
- fixtures in ancestor `conftest.py` files

Not implemented yet:

- fixture imports/re-exports across helper modules
- plugin-provided fixtures
- dynamic pytest fixture generation
- incremental indexing


## Releasing

Push a version tag to build release binaries:

```sh
git tag v0.1.0
git push origin v0.1.0
```

GitHub Actions publishes tarballs for:

```text
pyfixy-lsp-aarch64-apple-darwin.tar.gz
pyfixy-lsp-x86_64-unknown-linux-gnu.tar.gz
pyfixy-lsp-aarch64-unknown-linux-gnu.tar.gz
```
