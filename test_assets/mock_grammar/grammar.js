/// @file Mock grammar for Catenary integration tests.
/// Supports `fn name`, `struct name`, and brace-delimited blocks.
/// Used by test_assets/mock_grammar/ — regenerate with:
///   cd test_assets/mock_grammar && tree-sitter generate

module.exports = grammar({
  name: "mock",

  rules: {
    source_file: ($) => repeat($._definition),

    _definition: ($) => choice($.function_definition, $.struct_definition),

    function_definition: ($) =>
      seq("fn", $.identifier, optional($.block)),

    struct_definition: ($) =>
      seq("struct", $.identifier, optional($.block)),

    block: ($) => seq("{", repeat($._block_item), "}"),

    _block_item: ($) => choice($._definition, $.content),

    identifier: ($) => /[a-zA-Z_][a-zA-Z0-9_]*/,

    // Single non-whitespace token that is not a brace. Kept short so the
    // lexer prefers keyword tokens (`fn`, `struct`) over content when both
    // could match at the same position.
    content: ($) => token(prec(-1, /[^\s{}]+/)),
  },
});
