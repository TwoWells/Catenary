// Minimal tree-sitter grammar for testing `catenary install`.
// Grammar: source_file -> (empty). Accepts only empty documents.
// Exports: tree_sitter_mock() -> const TSLanguage *

#include "tree_sitter/parser.h"

#define LANGUAGE_VERSION 14
#define STATE_COUNT 2
#define LARGE_STATE_COUNT 2
#define SYMBOL_COUNT 2
#define ALIAS_COUNT 0
#define TOKEN_COUNT 1
#define EXTERNAL_TOKEN_COUNT 0
#define FIELD_COUNT 0
#define MAX_ALIAS_SEQUENCE_LENGTH 0
#define PRODUCTION_ID_COUNT 1

enum ts_symbol_identifiers {
  sym_source_file = 1,
};

static const char *const ts_symbol_names[] = {
  [ts_builtin_sym_end] = "end",
  [sym_source_file] = "source_file",
};

static const TSSymbolMetadata ts_symbol_metadata[] = {
  [ts_builtin_sym_end] = {
    .visible = false,
    .named = true,
  },
  [sym_source_file] = {
    .visible = true,
    .named = true,
  },
};

static const TSSymbol ts_symbol_map[] = {
  [ts_builtin_sym_end] = ts_builtin_sym_end,
  [sym_source_file] = sym_source_file,
};

static bool ts_lex(TSLexer *lexer, TSStateId state) {
  START_LEXER();
  switch (state) {
    case 0:
      ACCEPT_TOKEN(ts_builtin_sym_end);
      END_STATE();
    default:
      return false;
  }
}

// Parse actions:
//   [0] = error sentinel
//   [1..2] = REDUCE(source_file, 0 children)
//   [3..4] = SHIFT to state 1 (goto after reduce)
//   [5..6] = ACCEPT
static const TSParseActionEntry ts_parse_actions[] = {
  [0] = {.entry = {.count = 0, .reusable = false}},
  [1] = {.entry = {.count = 1, .reusable = true}}, REDUCE(sym_source_file, 0, 0, 0),
  [3] = {.entry = {.count = 1, .reusable = true}}, SHIFT(1),
  [5] = {.entry = {.count = 1, .reusable = true}}, ACCEPT_INPUT(),
};

// State 0: see END -> reduce to source_file; see source_file -> goto state 1
// State 1: see END -> accept
static const uint16_t ts_parse_table[LARGE_STATE_COUNT][SYMBOL_COUNT] = {
  [0] = {
    [ts_builtin_sym_end] = ACTIONS(1),
    [sym_source_file] = ACTIONS(3),
  },
  [1] = {
    [ts_builtin_sym_end] = ACTIONS(5),
  },
};

static const TSLexMode ts_lex_modes[STATE_COUNT] = {
  [0] = {.lex_state = 0},
  [1] = {.lex_state = 0},
};

static const TSStateId ts_primary_state_ids[STATE_COUNT] = {
  [0] = 0,
  [1] = 1,
};

extern const TSLanguage *tree_sitter_mock(void) {
  static const TSLanguage language = {
    .version = LANGUAGE_VERSION,
    .symbol_count = SYMBOL_COUNT,
    .alias_count = ALIAS_COUNT,
    .token_count = TOKEN_COUNT,
    .external_token_count = EXTERNAL_TOKEN_COUNT,
    .state_count = STATE_COUNT,
    .large_state_count = LARGE_STATE_COUNT,
    .production_id_count = PRODUCTION_ID_COUNT,
    .field_count = FIELD_COUNT,
    .max_alias_sequence_length = MAX_ALIAS_SEQUENCE_LENGTH,
    .parse_table = &ts_parse_table[0][0],
    .small_parse_table = NULL,
    .small_parse_table_map = NULL,
    .parse_actions = ts_parse_actions,
    .symbol_names = ts_symbol_names,
    .field_names = NULL,
    .field_map_slices = NULL,
    .field_map_entries = NULL,
    .symbol_metadata = ts_symbol_metadata,
    .public_symbol_map = ts_symbol_map,
    .alias_map = NULL,
    .alias_sequences = NULL,
    .lex_modes = ts_lex_modes,
    .lex_fn = ts_lex,
    .keyword_lex_fn = NULL,
    .keyword_capture_token = 0,
    .external_scanner = {0},
    .primary_state_ids = ts_primary_state_ids,
  };
  return &language;
}
