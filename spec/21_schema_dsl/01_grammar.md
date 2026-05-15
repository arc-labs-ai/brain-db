# Schema DSL — Formal Grammar

EBNF-style grammar for the the knowledge layer schema DSL.

## Top level

```ebnf
schema           := whitespace? (statement whitespace?)*
statement        := namespace_decl
                  | use_decl
                  | entity_type_def
                  | predicate_def
                  | relation_type_def
                  | extractor_def
                  | comment

comment          := "#" any_char* newline
```

## Namespace and imports

```ebnf
namespace_decl   := "namespace" ws identifier

use_decl         := "use" ws qualified_identifier
                    # e.g. `use brain.entity_mentions`

qualified_identifier := identifier ("." identifier)*
```

## Entity types

```ebnf
entity_type_def  := "define" ws "entity_type" ws identifier ws? "{" ws? 
                       attributes_block? 
                    ws? "}"

attributes_block := "attributes" ws? "{" ws? (attribute_decl ws?)* "}"

attribute_decl   := identifier ":" ws? attr_type (ws modifier)*
attr_type        := "text"
                  | "number"
                  | "bool"
                  | "date"
                  | "timestamp"
                  | "enum" ws? "[" ws? identifier_list ws? "]"
                  | "ref" ws? "<" ws? identifier ws? ">"
modifier         := "required" | "optional" | "unique" | "indexed"
                  | "default" ws literal
identifier_list  := identifier (ws? "," ws? identifier)*
```

## Predicates

```ebnf
predicate_def    := "define" ws "predicate" ws qualified_identifier ws? "{" ws?
                       "kind:" ws? statement_kind ws?
                       "object:" ws? object_type ws?
                       ("description:" ws? string_literal ws?)?
                    "}"

statement_kind   := "Fact" | "Preference" | "Event"

object_type      := "Value" ws? "<" ws? attr_type ws? ">"
                  | "Entity" ws? "<" ws? identifier ws? ">"
                  | "Memory"
                  | "Statement"
                  | "Any"     # falls back to Value<text> at storage level
```

## Relation types

```ebnf
relation_type_def := "define" ws "relation_type" ws identifier ws? "{" ws?
                       "from:" ws? identifier ws?
                       "to:" ws? identifier ws?
                       ("cardinality:" ws? cardinality ws?)?
                       ("symmetric:" ws? bool_literal ws?)?
                       ("properties" ws? "{" ws? (attribute_decl ws?)* "}")?
                    "}"

cardinality      := "one-to-one"
                  | "one-to-many"
                  | "many-to-one"
                  | "many-to-many"
```

## Extractors

```ebnf
extractor_def    := "define" ws "extractor" ws identifier ws? "{" ws?
                       "kind:" ws? extractor_kind ws?
                       "target:" ws? target_decl ws?
                       (extractor_field ws?)*
                    "}"

extractor_kind   := "pattern" | "classifier" | "llm"

target_decl      := "entity" ws identifier                    # produces entities of this type
                  | "statement" ws statement_kind             # produces statements of this kind
                  | "relation" ws identifier                  # produces relations of this type
                  | "entity_or_statement"                     # produces either; type inferred

extractor_field  := "patterns" ws? "[" ws? (regex_literal ws?)* "]"     # pattern only
                  | "model:" ws? string_literal                          # classifier/llm
                  | "feature_extraction:" ws? ("builtin" | identifier)   # classifier
                  | "prompt:" ws? heredoc_or_string                      # llm
                  | "examples:" ws? json_array                           # llm
                  | "schema:" ws? json_object                            # llm output schema
                  | "cache:" ws? ("enabled" | "disabled")                # llm
                  | "cache_ttl:" ws? duration                            # llm
                  | "confidence:" ws? number                             # pattern fixed conf
                  | "confidence_threshold:" ws? number                   # classifier/llm
                  | "trigger:" ws? trigger_expr
                  | "cost_budget:" ws? cost_expr                         # llm
                  | "depends_on:" ws? "[" ws? identifier_list ws? "]"
                  | "resolver" ws? "{" ws? resolver_config_fields "}"
```

## Triggers

```ebnf
trigger_expr     := "on" ws "encode"
                  | "on" ws "encode" ws "where" ws condition_expr
                  | "on" ws "demand"
                  | "on" ws "schema_change"
                  | "periodic" ws "at" ws cron_string

condition_expr   := condition_atom (ws ("and" | "or") ws condition_atom)*

condition_atom   := field_ref ws op ws value_expr
                  | field_ref ws "matches" ws regex_literal
                  | "(" ws? condition_expr ws? ")"

field_ref        := identifier ("." identifier)*
op               := "=" | "!=" | "<" | "<=" | ">" | ">=" | "in"
```

## Literals

```ebnf
identifier       := letter (letter | digit | "_")*
letter           := "a".."z" | "A".."Z"
digit            := "0".."9"

number           := digit+ ("." digit+)?
bool_literal     := "true" | "false"
string_literal   := '"' (any_char except '"' or escape)* '"'
heredoc_or_string := '"""' any_char* '"""'
                   | string_literal
regex_literal    := "/" any_char_except_unescaped_slash+ "/"
duration         := digit+ ("s" | "m" | "h" | "d")
cost_expr        := "$" number ws "per" ws ("memory" | "request" | "day")
cron_string      := string_literal                  # validated as cron at parse time
json_object      := any well-formed JSON object
json_array       := any well-formed JSON array

literal          := number | string_literal | bool_literal
```

## Whitespace and comments

```ebnf
ws               := (" " | "\t" | "\n" | "\r" | comment)+
whitespace       := ws
newline          := "\n" | "\r\n"
```

## Examples covered by this grammar

All examples in `00_purpose.md` parse under this grammar. Edge cases the parser must handle:

- Mixed CRLF/LF line endings (treated identically).
- Comments anywhere except inside string/regex literals.
- Trailing commas in lists and blocks (allowed and ignored).
- UTF-8 in string literals.
- Escaped characters in strings (`\n`, `\t`, `\"`, `\\`).
- Heredoc strings (`"""..."""`) for multiline prompts.

## Parser implementation choice

Use `pest` (PEG parser generator) for the implementation. Reasons:
- Grammar is mostly LL parseable; PEG is a natural fit.
- Pest grammar file mirrors EBNF closely; readability preserved.
- Good error messages out of the box.

`nom` is an alternative; either works.

## What's NOT in the grammar

- Comments inside JSON objects (use `description` field instead).
- Procedural code blocks.
- Imports of other schema files (single-file schema for the knowledge layer; multi-file is future versions).
- Conditional compilation / preprocessor (no `if` / `else` at the schema level).
