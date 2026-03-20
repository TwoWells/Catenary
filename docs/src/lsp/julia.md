# Julia

## Install

### macOS / Linux / Windows

From the Julia REPL:

```julia
using Pkg
Pkg.add("LanguageServer")
```

## Config

Add to `~/.config/catenary/config.toml`:

```toml
[server.julia]
command = "julia"
args = ["--startup-file=no", "--history-file=no", "-e", "using LanguageServer; runserver()"]
```

## Notes

- The server starts a Julia process, which has some startup time
- `--startup-file=no` and `--history-file=no` speed up startup
- First run on a project may take time to load packages and index
- Works best with projects that have a `Project.toml`

## Reducing Startup Time

For faster startup, you can create a custom sysimage:

```julia
using PackageCompiler
create_sysimage([:LanguageServer], sysimage_path="languageserver.so")
```

Then use:

```toml
[server.julia]
command = "julia"
args = ["--sysimage=/path/to/languageserver.so", "-e", "using LanguageServer; runserver()"]
```

## Links

- [LanguageServer.jl](https://github.com/julia-vscode/LanguageServer.jl)
- [Julia](https://julialang.org/)
