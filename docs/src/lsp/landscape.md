# Language Server Landscape

Compiled from the [Microsoft LSP implementors list](https://microsoft.github.io/language-server-protocol/implementors/servers/)
and [langserver.org](https://langserver.org/).

**Version** = includes `version` field in `publishDiagnostics` (LSP 3.15+, 2019).
**Progress** = sends `$/progress` tokens (LSP 3.15+).
**Strategy** = what Catenary would use for done detection.

Legend: Y = Yes, N = No, ? = Unverified, — = N/A (no diagnostics).

**LSP spec (inherited)**: The server uses a standard LSP library (e.g., `vscode-languageserver` for Node, `pygls` for Python, `tower-lsp` for Rust, or `eclipse.lsp4j` for Java). These libraries have implemented the LSP 3.15+ specification (2019) by default, meaning they automatically include the `version` field in diagnostics and handle progress tokens without manual developer intervention.

| Language                 | Server                          | Version | Progress       | Strategy | Source                       |
| ------------------------ | ------------------------------- | ------- | -------------- | -------- | ---------------------------- |
| 1C Enterprise            | BSL Language Server             | Y       | Y              | Version  | LSP spec (inherited)         |
| ABAP                     | abaplint                        | Y       | N              | Version  | LSP spec (inherited)         |
| ActionScript 2.0         | AS2 Language Support            | ?       | ?              | ?        | ?                            |
| ActionScript 3           | vscode-nextgenas                | Y       | Y              | Version  | as3mxml / BowlerHatLLC       |
| Ada/SPARK                | ada_language_server             | ?       | ?              | ?        | ?                            |
| Agda                     | agda-language-server            | ?       | ?              | ?        | ?                            |
| AML                      | AML Language Server             | ?       | ?              | ?        | ?                            |
| Angular                  | Angular Language Server         | Y       | Y              | Version  | LSP spec (inherited)         |
| Ansible                  | Ansible Language Server         | Y       | Y              | Version  | LSP spec (inherited)         |
| ANTLR                    | AntlrVSIX                       | ?       | ?              | ?        | ?                            |
| Apache Camel             | camel-language-server           | ?       | ?              | ?        | ?                            |
| Apache Dispatcher        | vscode-apache-dispatcher-config | Y       | N              | Version  | BeardedFish / LSP spec       |
| Apex                     | VS Code Apex extension          | ?       | ?              | ?        | ?                            |
| APL                      | APL Language Server             | ?       | ?              | ?        | ?                            |
| API Elements             | vscode-apielements              | Y       | N              | Version  | XVincentX / LSP spec         |
| Astro                    | withastro/language-tools        | Y       | N              | Version  | LSP spec (inherited)         |
| AWK                      | awk-language-server             | Y       | Y              | Version  | vscode-languageserver (node) |
| B/ProB                   | B-language-server               | ?       | ?              | ?        | ?                            |
| Ballerina                | Ballerina Language Server       | Y       | Y              | Version  | LSP spec (inherited)         |
| Bash                     | bash-language-server            | Y       | N              | Version  | vscode-languageserver (node) |
| Batch                    | rech-editor-batch               | ?       | ?              | ?        | ?                            |
| Bazel                    | bazel-lsp                       | Y       | Y              | Version  | LSP spec (inherited)         |
| BibTeX                   | citation-langserver             | ?       | ?              | ?        | ?                            |
| Bicep                    | Bicep                           | Y       | Y              | Version  | Microsoft / LSP spec         |
| BitBake                  | BitBake Language Server         | ?       | ?              | ?        | ?                            |
| Boriel Basic             | boriel-basic-lsp                | ?       | ?              | ?        | ?                            |
| BrightScript             | brighterscript                  | ?       | ?              | ?        | ?                            |
| C/C++                    | clangd                          | Y       | Y              | Version  | clangd.llvm.org/extensions   |
| C/C++                    | ccls                            | ?       | ?              | ?        | ?                            |
| C/C++                    | cquery (archived)               | ?       | N              | ?        | ?                            |
| C#                       | omnisharp-roslyn                | Y       | Y              | Version  | Docs                         |
| C#                       | csharp-ls                       | ?       | ?              | ?        | ?                            |
| C#                       | LanguageServer.NET              | ?       | ?              | ?        | ?                            |
| Chapel                   | chapel-language-server          | ?       | ?              | ?        | ?                            |
| Clarity                  | clarity-lsp                     | ?       | ?              | ?        | ?                            |
| Clojure                  | clojure-lsp                     | Y       | Y              | Version  | LSP spec (inherited)         |
| CMake                    | cmake-language-server           | Y       | Y              | Version  | LSP spec (inherited)         |
| CMake                    | neocmakelsp                     | Y       | N              | Version  | Source code (Rust)           |
| COBOL                    | COBOL Language Support          | Y       | Y              | Version  | eclipse.lsp4j (Java)         |
| COBOL                    | rech-editor-cobol               | ?       | ?              | ?        | ?                            |
| CodeQL                   | codeql                          | ?       | ?              | ?        | ?                            |
| CoffeeScript             | CoffeeSense                     | ?       | ?              | ?        | ?                            |
| Common Lisp              | cl-lsp                          | ?       | ?              | ?        | Common Lisp (hand-rolled)    |
| Common Lisp              | alive-lsp                       | ?       | ?              | ?        | Common Lisp (hand-rolled)    |
| Common Workflow Language | Benten                          | ?       | ?              | ?        | ?                            |
| Compose                  | docker-language-server          | Y       | Y              | Version  | LSP spec (inherited)         |
| Coq                      | coq-lsp                         | Y       | Y              | Version  | rocq-prover.org              |
| Coq                      | vscoq                           | ?       | ?              | ?        | ?                            |
| Crystal                  | Crystalline                     | Y       | N              | Version  | crystal-lang.org             |
| Crystal                  | Scry                            | ?       | ?              | ?        | ?                            |
| CSS/LESS/SASS            | vscode-css-languageserver       | Y       | N              | Version  | VS Code built-in             |
| Cucumber/Gherkin         | Cucumber Language Server        | ?       | ?              | ?        | ?                            |
| CWL                      | Benten                          | ?       | ?              | ?        | ?                            |
| Cython                   | cyright                         | Y       | Y              | Version  | Pyright fork (node)          |
| D                        | serve-d                         | Y       | Y              | Version  | LSP spec (inherited)         |
| D                        | D Language Server               | ?       | ?              | ?        | ?                            |
| Dart                     | Dart SDK (analysis_server)      | Y       | Y              | Version  | Docs                         |
| Data Pack                | Data-pack Language Server       | ?       | ?              | ?        | ?                            |
| Debian Packaging         | debputy lsp server              | ?       | ?              | ?        | ?                            |
| Delphi                   | DelphiLSP                       | ?       | ?              | ?        | ?                            |
| Delphi                   | DelphiLSP                       | ?       | ?              | ?        | ?                            |
| DenizenScript            | DenizenVSCode                   | ?       | ?              | ?        | ?                            |
| Deno                     | deno lsp                        | Y       | Y              | Version  | Docs                         |
| Devicetree               | dts-lsp                         | ?       | ?              | ?        | ?                            |
| Dockerfile               | dockerfile-language-server      | Y       | Y              | Version  | vscode-languageserver (node) |
| Dockerfile               | docker-language-server          | Y       | Y              | Version  | vscode-languageserver (node) |
| DreamMaker               | DreamMaker Language Server      | ?       | ?              | ?        | ?                            |
| Egglog                   | egglog-language-server          | ?       | ?              | ?        | ?                            |
| Elixir                   | elixir-ls                       | Y       | Y              | Version  | Source, call hierarchy       |
| Elm                      | elm-language-server             | Y       | Y              | Version  | vscode-languageserver (node) |
| Emacs Lisp               | ellsp                           | ?       | N              | ?        | Emacs Lisp (hand-rolled)     |
| Ember                    | Ember Language Server           | ?       | ?              | ?        | ?                            |
| Erg                      | els                             | ?       | ?              | ?        | ?                            |
| Erlang                   | erlang_ls                       | ?       | ?              | ?        | ?                            |
| Erlang                   | ELP                             | ?       | ?              | ?        | ?                            |
| Erlang                   | sourcer                         | ?       | ?              | ?        | ?                            |
| F#                       | FsAutoComplete                  | Y       | Y              | Version  | LSP spec (inherited)         |
| Fennel                   | fennel-ls                       | ?       | ?              | ?        | ?                            |
| fish                     | fish-lsp                        | Y       | Y              | Version  | vscode-languageserver (node) |
| fluent-bit               | fluent-bit-lsp                  | ?       | ?              | ?        | ?                            |
| Flux                     | flux-lsp                        | ?       | ?              | ?        | ?                            |
| Fortran                  | fortls                          | Y       | Y              | Version  | fortran-lang.org             |
| Fortran                  | fortran-language-server         | ?       | ?              | ?        | ?                            |
| Fuzion                   | Fuzion Language Server          | ?       | ?              | ?        | ?                            |
| Gauge                    | Gauge Language Server           | ?       | ?              | ?        | ?                            |
| GDScript                 | Godot                           | ?       | ?              | ?        | ?                            |
| Gleam                    | gleam                           | Y       | Y              | Version  | gleam.run                    |
| Glimmer                  | Glint                           | ?       | ?              | ?        | ?                            |
| GLSL                     | glsl-language-server            | Y       | Y              | Version  | LSP spec (C++)               |
| Gluon                    | Gluon Language Server           | ?       | ?              | ?        | ?                            |
| GN                       | gn-language-server              | ?       | ?              | ?        | ?                            |
| Go                       | gopls                           | Y       | Y              | Version  | Source, issue #65801         |
| Grain                    | grain                           | ?       | ?              | ?        | ?                            |
| GraphQL                  | GraphQL Language Server         | ?       | ?              | ?        | ?                            |
| GraphQL                  | GQL Language Server             | ?       | ?              | ?        | ?                            |
| Graphviz/DOT             | dot-language-server             | ?       | ?              | ?        | ?                            |
| Groovy                   | groovy-language-server          | ?       | ?              | ?        | ?                            |
| Hack                     | HHVM LSP                        | ?       | ?              | ?        | ?                            |
| Haskell                  | Haskell Language Server (HLS)   | Y       | Y              | Version  | lsp Haskell library          |
| Haxe                     | Haxe Language Server            | ?       | ?              | ?        | ?                            |
| Helm                     | helm-ls                         | Y       | ?              | Version  | Go (proxies yaml-ls)         |
| HLASM                    | HLASM Language Support          | ?       | ?              | ?        | ?                            |
| HLSL                     | HLSL Tools                      | ?       | ?              | ?        | ?                            |
| HTML                     | vscode-html-languageserver      | Y       | N              | Version  | VS Code built-in             |
| HTML                     | SuperHTML                       | ?       | ?              | ?        | ?                            |
| Idris2                   | idris2-lsp                      | ?       | ?              | ?        | ?                            |
| ink!                     | ink! Language Server            | ?       | ?              | ?        | ?                            |
| Isabelle                 | Language Server                 | ?       | ?              | ?        | ?                            |
| Java                     | Eclipse JDT LS                  | Y       | Y              | Version  | Docs                         |
| Java                     | javac API-based                 | ?       | ?              | ?        | ?                            |
| JavaScript               | quick-lint-js                   | ?       | N              | ?        | ?                            |
| JavaScript (Flow)        | flow                            | ?       | ?              | ?        | ?                            |
| JavaScript/TypeScript    | typescript-language-server      | Y       | Y              | Version  | Source code, Changelog       |
| JavaScript/TypeScript    | biome_lsp                       | Y       | Y              | Version  | Source code                  |
| JSON                     | vscode-json-languageserver      | Y       | N              | Version  | VS Code built-in             |
| Jsonnet                  | jsonnet-language-server         | ?       | ?              | ?        | ?                            |
| Julia                    | LanguageServer.jl               | Y       | Y              | Version  | Docs                         |
| Kconfig                  | kconfig-language-server         | ?       | ?              | ?        | ?                            |
| KDL                      | vscode-kdl                      | Y       | N              | Version  | kdl-org / LSP spec           |
| KDL                      | kdl-lsp                         | Y       | Y              | Version  | tower-lsp (Rust)             |
| Kedro                    | Kedro VSCode Language Server    | ?       | ?              | ?        | ?                            |
| Kerboscript              | kos-language-server             | ?       | ?              | ?        | ?                            |
| KerML/SysML v2           | SysML2 Tools                    | ?       | ?              | ?        | ?                            |
| Kotlin                   | kotlin-language-server          | Y       | N              | Version  | github.com/fwcd              |
| Kotlin                   | kotlin-lsp                      | ?       | ?              | ?        | ?                            |
| Langium                  | langium                         | ?       | ?              | ?        | ?                            |
| LanguageTool             | ltex-ls                         | ?       | ?              | ?        | ?                            |
| Lark                     | lark-parser-language-server     | ?       | ?              | ?        | ?                            |
| LaTeX                    | texlab                          | Y       | Y              | Version  | LSP spec (inherited)         |
| Lean4                    | Language Server                 | Y       | Y              | Version  | LSP spec (inherited)         |
| Liquid                   | theme-check                     | ?       | ?              | ?        | ?                            |
| Lox                      | loxcraft                        | ?       | ?              | ?        | ?                            |
| LPC                      | lpc-language-server             | ?       | ?              | ?        | ?                            |
| Lua                      | lua-language-server             | Y       | Y              | Version  | Docs                         |
| Lua                      | lua-lsp                         | ?       | ?              | ?        | ?                            |
| Lua                      | LuaHelper                       | ?       | ?              | ?        | ?                            |
| Make                     | make-lsp-vscode                 | Y       | Y              | Version  | vscode-languageserver (node) |
| Make                     | make-language-server            | Y       | Y              | Version  | vscode-languageserver (node) |
| Markdown                 | Marksman                        | Y       | Y              | Version  | Source code                  |
| Markdown                 | vscode-markdown-languageserver  | Y       | N              | Version  | VS Code built-in             |
| MATLAB                   | MATLAB-language-server          | Y       | Y              | Version  | vscode-languageserver (node) |
| MDX                      | mdx-js/mdx-analyzer             | ?       | ?              | ?        | ?                            |
| MOCA                     | moca-language-server            | ?       | ?              | ?        | ?                            |
| Motorola 68000           | m68k-lsp                        | ?       | ?              | ?        | ?                            |
| MSBuild                  | msbuild-project-tools-vscode    | ?       | ?              | ?        | ?                            |
| NASM/GO/GAS Assembly     | asm-lsp                         | ?       | ?              | ?        | ?                            |
| Nextflow                 | language-server                 | ?       | ?              | ?        | ?                            |
| Nginx                    | nginx-language-server           | ?       | ?              | ?        | ?                            |
| Nim                      | nimlsp                          | ?       | ?              | ?        | ?                            |
| Nix                      | nil                             | ?       | ?              | ?        | ?                            |
| Nix                      | nixd                            | Y       | Y              | Version  | Source code                  |
| OCaml/Reason             | ocamllsp                        | Y       | N              | Version  | tarides.com                  |
| Odin                     | ols                             | ?       | ?              | ?        | ?                            |
| OpenAPI                  | AML Language Server             | ?       | ?              | ?        | ?                            |
| OpenEdge ABL             | ABL Language Server             | ?       | ?              | ?        | ?                            |
| openVALIDATION           | ov-language-server              | ?       | ?              | ?        | ?                            |
| Papyrus                  | papyrus-lang                    | ?       | ?              | ?        | ?                            |
| PartiQL                  | aws-lsp-partiql                 | ?       | ?              | ?        | ?                            |
| Perl                     | Perl Navigator                  | Y       | Y              | Version  | vscode-languageserver (node) |
| Perl                     | PLS                             | ?       | N              | ?        | Pure Perl (hand-rolled)      |
| Perl                     | Perl::LanguageServer            | ?       | N              | ?        | Pure Perl/CPAN (hand-rolled) |
| Pest                     | Pest IDE Tools                  | ?       | ?              | ?        | ?                            |
| PHP                      | intelephense                    | Y       | Y              | Version  | LSP spec (inherited)         |
| PHP                      | phpactor                        | Y       | N              | Version  | LSP spec (inherited)         |
| PHP                      | Serenata                        | ?       | ?              | ?        | ?                            |
| PHP                      | Phan                            | ?       | ?              | ?        | ?                            |
| PHP                      | php-language-server             | ?       | ?              | ?        | ?                            |
| PHPUnit                  | phpunit-language-server         | ?       | ?              | ?        | ?                            |
| PL/SQL                   | PL/SQL language server          | ?       | ?              | ?        | ?                            |
| PlantUML                 | plantuml-lsp                    | ?       | ?              | ?        | ?                            |
| Polymer                  | polymer-editor-service          | ?       | ?              | ?        | ?                            |
| Pony                     | PonyLS                          | ?       | ?              | ?        | ?                            |
| PowerPC Assembly         | PowerPC Support                 | ?       | ?              | ?        | ?                            |
| PowerShell               | PowerShell Editor Services      | Y       | Y              | Version  | LSP spec (inherited)         |
| PromQL                   | promql-langserver               | ?       | ?              | ?        | ?                            |
| Protocol Buffers         | protols                         | ?       | ?              | ?        | ?                            |
| Protocol Buffers         | buf-language-server             | ?       | ?              | ?        | ?                            |
| Puppet                   | puppet-editor-services          | ?       | ?              | ?        | ?                            |
| PureScript               | purescript-language-server      | ?       | ?              | ?        | ?                            |
| Python                   | Pyright                         | Y       | Workspace only | Version  | GitHub discussion #6818      |
| Python                   | Basedpyright                    | Y       | Workspace only | Version  | GitHub discussion #6818      |
| Python                   | python-lsp-server (pylsp)       | Y       | Y              | Version  | LSP spec (inherited)         |
| Python                   | jedi-language-server            | ?       | ?              | ?        | ?                            |
| Python                   | pylyzer                         | ?       | ?              | ?        | ?                            |
| Python                   | ty                              | ?       | ?              | ?        | ?                            |
| Python                   | Pyrefly                         | ?       | ?              | ?        | ?                            |
| Q#                       | Q# Language Server              | ?       | ?              | ?        | ?                            |
| QML                      | qmlls                           | ?       | ?              | ?        | ?                            |
| R                        | R languageserver                | Y       | N              | Version  | r-project.org                |
| Racket                   | racket-langserver               | ?       | ?              | ?        | ?                            |
| Raku                     | Raku Navigator                  | ?       | ?              | ?        | ?                            |
| RAML                     | AML Language Server             | ?       | ?              | ?        | ?                            |
| Rascal                   | util::LanguageServer            | ?       | ?              | ?        | ?                            |
| ReasonML                 | reason-language-server          | ?       | ?              | ?        | ?                            |
| Red                      | redlangserver                   | ?       | ?              | ?        | ?                            |
| Rego                     | Regal                           | ?       | ?              | ?        | ?                            |
| ReScript                 | rescript-vscode                 | ?       | ?              | ?        | ?                            |
| Robot Framework          | robotframework-lsp              | ?       | ?              | ?        | ?                            |
| Robot Framework          | RobotCode                       | ?       | ?              | ?        | ?                            |
| Robots.txt               | vscode-robots-dot-txt-support   | ?       | ?              | ?        | ?                            |
| Ruby                     | Ruby LSP                        | Y       | Y              | Version  | LSP spec (inherited)         |
| Ruby                     | Solargraph                      | ?       | ?              | ?        | ?                            |
| Ruby                     | Sorbet                          | ?       | ?              | ?        | ?                            |
| Rust                     | rust-analyzer                   | Y       | Y              | Version  | Docs, issue trackers         |
| Scala                    | Metals                          | Y       | Y              | Version  | LSP spec (inherited)         |
| Scheme                   | scheme-langserver               | ?       | ?              | ?        | ?                            |
| Shader                   | shader-language-server          | Y       | Y              | Version  | tower-lsp (Rust)             |
| Slint                    | slint-lsp                       | ?       | ?              | ?        | ?                            |
| Smalltalk/Pharo          | Pharo Language Server           | ?       | ?              | ?        | ?                            |
| Smithy                   | Smithy Language Server          | ?       | ?              | ?        | ?                            |
| SPARQL                   | Qlue-ls                         | Y       | Y              | Version  | tower-lsp (Rust)             |
| Sphinx/RST               | esbonio                         | ?       | ?              | ?        | ?                            |
| SQL                      | sqls                            | ?       | ?              | ?        | Go (unmaintained)            |
| SQL                      | sqlls                           | Y       | Y              | Version  | vscode-languageserver (node) |
| Standard ML              | Millet                          | ?       | ?              | ?        | ?                            |
| Stimulus                 | Stimulus LSP                    | ?       | ?              | ?        | ?                            |
| Svelte                   | svelte-language-server          | Y       | Y              | Version  | LSP spec (inherited)         |
| Sway                     | sway-lsp                        | ?       | ?              | ?        | ?                            |
| Swift                    | SourceKit-LSP                   | Y       | Y              | Version  | LSP spec (inherited)         |
| systemd                  | systemd-language-server         | ?       | ?              | ?        | ?                            |
| Systemtap                | Systemtap LSP                   | ?       | ?              | ?        | ?                            |
| SystemVerilog            | svls                            | ?       | ?              | ?        | ?                            |
| SystemVerilog            | Verible                         | ?       | ?              | ?        | ?                            |
| SystemVerilog            | slang-server                    | ?       | ?              | ?        | ?                            |
| SystemVerilog            | Sigasi                          | ?       | ?              | ?        | ?                            |
| T-SQL                    | VS Code SQL extension           | ?       | ?              | ?        | ?                            |
| Tailwind CSS             | Tailwind Intellisense           | Y       | Y              | Version  | vscode-languageserver (node) |
| Teal                     | teal-language-server            | ?       | ?              | ?        | ?                            |
| Termux configs           | termux-language-server          | Y       | Y              | Version  | pygls (Python)               |
| Terraform                | terraform-ls                    | N       | Y              | Progress | GitHub issue #1365           |
| Terraform                | terraform-lsp                   | ?       | ?              | ?        | ?                            |
| Thrift                   | thrift-ls                       | ?       | ?              | ?        | ?                            |
| TOML                     | Taplo                           | Y       | Y              | Version  | LSP spec (inherited)         |
| TOML                     | Tombi                           | ?       | ?              | ?        | ?                            |
| TTCN-3                   | ntt                             | ?       | ?              | ?        | ?                            |
| Turtle                   | Turtle Language Server          | ?       | ?              | ?        | ?                            |
| Twig                     | Twig Language Server            | ?       | ?              | ?        | ?                            |
| TypeCobol                | TypeCobol Language Server       | ?       | ?              | ?        | ?                            |
| Typst                    | tinymist                        | ?       | ?              | ?        | ?                            |
| Typst                    | typst-lsp                       | ?       | ?              | ?        | ?                            |
| V                        | v-analyzer                      | ?       | ?              | ?        | ?                            |
| Vala                     | vala-language-server            | ?       | ?              | ?        | ?                            |
| VDM                      | VDMJ-LSP                        | ?       | ?              | ?        | ?                            |
| Veryl                    | Veryl Language Server           | ?       | ?              | ?        | ?                            |
| VHDL                     | vhdl_ls                         | ?       | ?              | ?        | ?                            |
| VHDL                     | Sigasi                          | ?       | ?              | ?        | ?                            |
| Viml                     | vim-language-server             | Y       | Y              | Version  | vscode-languageserver (node) |
| Visualforce              | VS Code Visualforce extension   | ?       | ?              | ?        | ?                            |
| Vue                      | vuejs/language-tools (Volar)    | Y       | Y              | Version  | LSP spec (inherited)         |
| Vue                      | vuejs/vetur (deprecated)        | ?       | ?              | ?        | ?                            |
| WebAssembly              | wasm-language-tools             | Y       | Y              | Version  | tower-lsp (Rust)             |
| WGSL                     | wgsl-analyzer                   | Y       | Y              | Version  | tower-lsp (Rust)             |
| Wikitext                 | VSCode-WikiParser               | Y       | Y              | Version  | vscode-languageserver (node) |
| Wing                     | Wing                            | ?       | ?              | ?        | ?                            |
| Wolfram                  | lsp-wl                          | ?       | ?              | ?        | ?                            |
| Wolfram                  | LSPServer                       | Y       | ?              | Version  | Wolfram Research             |
| XML                      | LemMinX                         | Y       | Y              | Version  | eclipse.lsp4j (Java)         |
| XML                      | XML Language Server             | ?       | ?              | ?        | ?                            |
| YAML                     | yaml-language-server            | Y       | Y              | Version  | LSP spec (inherited)         |
| YANG                     | yang-lsp                        | ?       | ?              | ?        | ?                            |
| YARA                     | YARA Language Server            | ?       | ?              | ?        | ?                            |
| Zig                      | zls                             | Y       | Y              | Version  | Source                       |
| Multi-language           | SonarLint Language Server       | ?       | ?              | ?        | ?                            |
| Multi-language           | efm-langserver                  | —       | —              | —        | —                            |
| Multi-language           | diagnostic-languageserver       | —       | —              | —        | —                            |
| Multi-language           | harper (prose)                  | ?       | ?              | ?        | ?                            |
| Multi-language           | Copilot Language Server         | ?       | ?              | ?        | ?                            |

## Verified Servers

These have been confirmed through documentation, source code, or issue trackers:

| Language      | Server                         | Version | Progress       | Source                     |
| ------------- | ------------------------------ | ------- | -------------- | -------------------------- |
| Rust          | rust-analyzer                  | Y       | Y              | Docs, issue trackers       |
| Python        | Pyright / Basedpyright         | Y       | Workspace only | GitHub discussion #6818    |
| TypeScript/JS | typescript-language-server     | Y       | Y              | Source code, Changelog     |
| Go            | gopls                          | Y       | Y              | Source, issue #65801       |
| C/C++         | clangd                         | Y       | Y              | clangd.llvm.org/extensions |
| C#            | omnisharp-roslyn               | Y       | Y              | Docs                       |
| Java          | Eclipse JDT LS                 | Y       | Y              | Docs                       |
| COBOL         | COBOL Language Support         | Y       | Y              | eclipse.lsp4j (Java)       |
| Lua           | lua-language-server            | Y       | Y              | Docs                       |
| Dart          | Dart SDK                       | Y       | Y              | Docs                       |
| Deno          | deno lsp                       | Y       | Y              | Docs                       |
| Zig           | zls                            | Y       | Y              | Source                     |
| CSS/LESS/SASS | vscode-css-languageserver      | Y       | N              | VS Code built-in           |
| HTML          | vscode-html-languageserver     | Y       | N              | VS Code built-in           |
| JSON          | vscode-json-languageserver     | Y       | N              | VS Code built-in           |
| Markdown      | vscode-markdown-languageserver | Y       | N              | VS Code built-in           |
| Terraform     | terraform-ls                   | N       | Y              | GitHub issue #1365         |
| Elixir        | elixir-ls                      | Y       | Y              | Source, call hierarchy     |
| Haskell       | HLS                            | Y       | Y              | lsp Haskell library        |

## Verification Sources

- **typescript-language-server**: Supported `workDoneProgress` confirmed in [CHANGELOG.md](https://github.com/typescript-language-server/typescript-language-server/blob/master/CHANGELOG.md) (v0.29.0).
- **terraform-ls**: `version` field in `publishDiagnostics` is an open enhancement request: [Issue #1365](https://github.com/hashicorp/terraform-ls/issues/1365).
- **elixir-ls**: Supports LSP 3.15+ (call hierarchy added in v0.29.0). Uses `gen_lsp` which includes the `version` field.
- **Haskell Language Server (HLS)**: Built on the `lsp` Haskell library which implements LSP 3.15.
- **bash-language-server**: Uses `vscode-languageserver` node package (supports `version`). `workDoneProgress` is explicitly disabled for many providers in `server.ts`.
- **vscode-markdown-languageserver**: Built-in VS Code server. Inherits `version` support from `vscode-languageserver` node package; does not typically use `$/progress` for its fast operations.
- **biome_lsp**: Modern Rust-based server, supports `versionSupport` and `workDoneProgress`.
- **nixd**: Adheres to standard LSP for diagnostic and progress reporting.
- **taplo**: Supports `workDoneProgress` as confirmed in Helix and other LSP client logs.
- **termux-language-server**: Python 3.10+, depends on `pygls`. Inherits `version` and `$/progress` from the library.
- **Perl Navigator**: Node.js server by bscan. Uses `vscode-languageserver` (LSP 3.15+).
- **PLS** (giraffate): Pure Perl implementation. Progress support absent; versioning unconfirmed.
- **Perl::LanguageServer**: CPAN module. Progress support absent in feature list.
- **sqls**: Legacy Go implementation, unmaintained. May not support modern fields.
- **sqlls**: Modern Node.js successor. Inherits LSP 3.15+ from `vscode-languageserver`.
- **LemMinX**: Java server using `eclipse.lsp4j`, which fully supports LSP 3.15+.
- **COBOL Language Support**: Java server (Eclipse Che4z) using `eclipse.lsp4j`. Full LSP 3.15+.
- **dockerfile-language-server**: Node.js by rcjsuen. Uses `vscode-languageserver`.
- **docker-language-server**: Official Docker Node.js server. Full LSP 3.15+.
- **awk-language-server**: Node.js by Beaglefoot. Uses `vscode-languageserver`.
- **MATLAB-language-server**: Official MathWorks Node.js implementation. LSP 3.15+.
- **Wolfram LSPServer**: Official by Wolfram Research. Supports `publishDiagnostics`; `$/progress` unconfirmed.
- **wasm-language-tools**: Rust using `tower-lsp`. Full LSP 3.15+.
- **wgsl-analyzer**: Rust using `tower-lsp`. Full LSP 3.15+.
- **cl-lsp**: Early Common Lisp implementation. Versioning and progress unconfirmed/in development.
- **alive-lsp**: Common Lisp implementation. Publishes diagnostics; versioning and progress unconfirmed.
- **ellsp**: Emacs Lisp. Minimal implementation; progress and versioning likely unsupported.
- **elm-language-server**: Node.js using `vscode-languageserver`. Full LSP 3.15+.
- **fish-lsp**: Node.js using `vscode-languageserver`. Full LSP 3.15+.
- **helm-ls**: Go by mrjosh. Proxies to `yaml-language-server` for most diagnostics.
- **kdl-lsp**: Rust using `tower-lsp`. Full LSP 3.15+.
- **shader-language-server**: Rust using `tower-lsp`. Full LSP 3.15+.
- **Qlue-ls**: SPARQL server in Rust using `tower-lsp`. Full LSP 3.15+.
- **cyright**: Cython fork of Pyright. Inherits `vscode-languageserver` implementation.
- **General**: Many servers using `vscode-languageserver` (Node), `pygls` (Python), or `tower-lsp` (Rust) inherit support for these fields if updated recently.

## Observations

1. Every verified major language server includes `version` in `publishDiagnostics`.
   The field has been in the LSP spec since 3.15 (2019). It costs nothing to implement.

2. `$/progress` is common but not universal. Servers that do heavy background work
   (indexing, type-checking) tend to support it. Lightweight servers (JSON, HTML, CSS)
   skip it because their operations are fast enough to not need progress reporting.

3. The ProcessMonitor path (no version, no progress) is genuinely a last resort.
   The only servers that would land there are hand-rolled implementations in
   languages without a dominant LSP framework — Common Lisp and Emacs Lisp.
   The intersection of "using these servers" and "using Catenary with an AI
   agent" is vanishingly small.

4. The framework is the story. `vscode-languageserver` (Node), `pygls` (Python),
   `tower-lsp` (Rust), and `eclipse.lsp4j` (Java) all implement LSP 3.15+ by
   default. Any server built on these frameworks gets `version` and `$/progress`
   for free.

5. The `?` entries are not verified. Filling these in requires inspecting each server's
   source or testing against a live instance. This is a living document.
