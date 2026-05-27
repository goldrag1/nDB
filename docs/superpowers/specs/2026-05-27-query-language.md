# nDB Query Language — v1 Working Spec

> **Status:** Decided 2026-05-27. Closes §12.9 of the main hypergraph design
> spec. Drives implementation of the `ndb-query` crate, the
> `ndb-engine::wire::QueryRequest` / `QueryResponse` AST, the server's
> `POST /query` route, and the matching client surfaces.

This document supersedes the open sub-questions in §12.9 of
`2026-05-27-nDB-hypergraph-design.md`. The parent spec's §12.1–§12.8 (paradigm,
wire format, surface syntax intent, embedded DSL sketch) stay authoritative;
this document fills in the BNF, semantics, error model, and v1 scope cut.

---

## 1. Scope cut for v1

| In v1 | Deferred |
|---|---|
| Pattern matching over entities + hyperedges, including recursive path patterns | Subqueries / CTEs |
| Filtering via `where` (boolean expressions over comparisons) | Aggregation (`sum`, `count`, `avg`) — lives in the slicer crate (§7) |
| Projection via `return` | Sorting (`order by`) — slicer |
| Result limiting via `limit` | Grouping (`group by`) — slicer |
| Time travel via `as of <expr>` prefix | Math expressions in `where` (only comparisons + boolean ops in v1) |
| Self-binding via `as ?var` suffix | Window functions, having, joins via SQL keywords |
| Recursive transitive closure: `type*`, `type+`, `type?`, `type{n,m}` | Index hints / planner directives — planner picks |
| READ-ONLY semantics | Writes through query language (use `/commit` instead) |
| Engine-side name → id resolution | Pre-resolved id-only wire (clients send names; server resolves) |
| Span-based error messages (line:col) | NL-to-AST input (client-side LLM concern, not engine) |

The deferred items are explicitly out of v1 — touching them would extend the
delivery window for the dominant missing piece without delivering proportional
value. They will be addressed once v1 ships and real query workloads inform
priority.

---

## 2. Locked design decisions

### 2.1 Paradigm and surface

- Declarative pattern matching, Datalog-influenced (§12.1 of parent spec).
- Surface syntax: **SQL-ish pattern functions** — `type_name(role: term, ...) as ?var`.
  Chosen over TypeQL-style `$x isa type`, bracket-record form, and YAML-block form
  because (a) it scales cleanly to high arity via role labels — the actual
  n-dimensional property of nDB — (b) is concise enough to handwrite and shell-embed,
  (c) LLMs generate it reliably, (d) is familiar to the largest dev audience.

### 2.2 Wire AST shape

Per the parent spec §12.2 + the user's locked instruction:

> Each pattern atom is `{kind, type_id, role_bindings, property_filters}`, joined by shared variables.

- `type_id`, `role_id`, `property_id` on the wire are u32 dictionary slots.
- The server resolves type/role/property NAMES (from the parser AST) to ids
  before constructing the wire payload — clients sending the surface text via
  `POST /query` get name resolution for free.
- Clients with a cached dictionary may send a pre-resolved id-based AST via
  `POST /query/ast` (alternate entry point; same planner; bypasses the parser).

### 2.3 Self-binding

`as ?var` suffix binds the entity or hyperedge UUID:

```
match
  diagnosis(patient: ?p, symptom: "fever") as ?diag1
  diagnosis(patient: ?p, symptom: "rash")  as ?diag2
where ?diag1 != ?diag2
return ?p, ?diag1, ?diag2
```

`id` is NOT a reserved key. If a schema has a property literally called `id`,
it behaves like any other property. The §12.6 examples that used `id:` as
"the entity's own UUID" are replaced with `as ?var`:

```
# Was (§12.6):  customer(id: ?cust, name: ?name, region: "Vietnam")
# Now:          customer(name: ?name, region: "Vietnam") as ?cust
```

### 2.4 Entity vs hyperedge pattern disambiguation

The parser does NOT know whether a type name refers to an entity type or a
hyperedge type — that's a schema decision. The parser produces a uniform
`Pattern { type_name, bindings, self_var }` AST. The **resolver** (server-side,
between parser and planner) inspects the schema dictionaries:

- If `type_name` refers to a hyperedge type → each binding is a `role_binding`.
- If `type_name` refers to an entity type → each binding is a `property_filter`.
- If both exist (rare; possible in a schemaless DB) → ambiguous-type error.
- If neither → unknown-type error.

This keeps the surface syntax uniform and pushes schema knowledge to one place.

### 2.5 NL input

Engine grammar is the only input path. Natural-language wrappers are a
client/SDK concern. Engine boundary stays deterministic, offline-capable, and
free of external service dependencies.

---

## 3. Grammar (EBNF)

```ebnf
query           = [ "as" "of" snapshot_expr ]
                  match_clause
                  [ where_clause ]
                  return_clause
                  [ limit_clause ]
                  ;

match_clause    = "match" pattern { pattern } ;

pattern         = type_ref [ recursion_suffix ] pattern_body [ self_bind ] ;
type_ref        = identifier ;
pattern_body    = "(" [ binding_list ] ")" ;
binding_list    = binding { "," binding } ;
binding         = identifier ":" term ;
self_bind       = "as" variable ;
recursion_suffix= "*" | "+" | "?" | "{" integer "," integer "}" ;
(* recursion_suffix comes BEFORE the pattern_body — matches §12.6 examples
   like `contains*(parent: body_42, child: ?leaf)` *)

where_clause    = "where" boolean_expr ;
boolean_expr    = and_expr { "or" and_expr } ;
and_expr        = not_expr { "and" not_expr } ;
not_expr        = [ "not" ] primary ;
primary         = comparison | "(" boolean_expr ")" ;
comparison      = term cmp_op term ;
cmp_op          = "=" | "!=" | "<" | "<=" | ">" | ">=" ;

return_clause   = "return" variable { "," variable } ;
limit_clause    = "limit" integer ;
snapshot_expr   = literal ;   (* number = tx_id; string = RFC3339 timestamp *)

term            = variable | literal ;
variable        = "?" identifier ;
literal         = string_lit | number_lit | bool_lit | null_lit | uuid_lit ;

identifier      = letter { letter | digit | "_" } ;
string_lit      = '"' { string_char } '"' ;
number_lit      = [ "-" ] digit { digit } [ "." digit { digit } ] ;
bool_lit        = "true" | "false" ;
null_lit        = "null" ;
uuid_lit        = "uuid:" canonical_uuid_form ;
integer         = digit { digit } ;
```

### 3.1 Operator precedence (highest first)

| Tier | Operators |
|---|---|
| 1 | `(` `)` grouping |
| 2 | unary `not` |
| 3 | `=` `!=` `<` `<=` `>` `>=` (non-associative) |
| 4 | `and` (left-associative) |
| 5 | `or` (left-associative) |

Comparison ops are non-associative — `a < b < c` is a syntax error, not a
chained comparison. Use `a < b and b < c`. This rules out the Python-style
implicit chain that has surprised generations of engineers.

No arithmetic in v1 — `where ?amt + ?fee > 1000` does not parse. Push math
into the slicer (§7) or precompute. If demand for in-query arithmetic
emerges, v2 adds it as Tier 2.5 between `not` and comparisons with the
standard `*`/`/` > `+`/`-` shape.

### 3.2 Comments

`# comment until newline` — identical to shell comments. No block comments
in v1; if needed, repeat the `#` per line. Comments are lexer-stripped.

### 3.3 Whitespace and newlines

Whitespace including newlines is purely a token separator. There is no
indentation grammar — patterns can be on one line or many. Trailing
commas inside `(...)` and in `return` lists are allowed.

### 3.4 Reserved words

`as`, `and`, `or`, `not`, `match`, `where`, `return`, `limit`, `of`, `true`,
`false`, `null`. Reserved words are case-insensitive on input; canonical form
is lower-case.

Type names, role names, property names, and variables can shadow reserved
words ONLY via context — e.g. a role called `where` would need a workaround
because it appears immediately after an identifier in `pattern_body`. For v1
we forbid this; any dictionary entry whose name matches a reserved word
fails name resolution with a clear error. v2 may add a quoted-identifier
form (`` `where` ``).

---

## 4. Wire AST

The server-side internal representation, post-name-resolution. Sent over
the wire by clients that have a dictionary cache; produced by the parser
for clients that submit text.

```jsonc
{
  "as_of": { "tx_id": 42 } | { "timestamp_us": 1700000000000000 } | null,
  "patterns": [
    {
      "kind": "entity",
      "type_id": 100,
      "self_var": "p",
      "property_filters": [
        { "property_id": 30, "op": "eq",
          "term": {"kind": "literal", "value": {"tag":"string","value":"fever"}} },
        { "property_id": 31, "op": "eq",
          "term": {"kind": "var", "name": "name"} }
      ]
    },
    {
      "kind": "hyperedge",
      "type_id": 200,
      "self_var": null,
      "role_bindings": [
        { "role_id": 10, "term": {"kind":"var","name":"p"} },
        { "role_id": 11, "term": {"kind":"literal","value":{"tag":"string","value":"fever"}} },
        { "role_id": 12, "term": {"kind":"var","name":"disease"} }
      ],
      "property_filters": [],
      "recursion": null
    },
    {
      "kind": "hyperedge",
      "type_id": 201,
      "self_var": null,
      "role_bindings": [
        { "role_id": 13, "term": {"kind":"literal","value":{"tag":"uuid","value":"..."}} },
        { "role_id": 14, "term": {"kind":"var","name":"leaf"} }
      ],
      "property_filters": [],
      "recursion": { "kind": "star", "max_depth": 64 }
    }
  ],
  "filter": {
    "kind": "cmp",
    "left":  {"kind":"var","name":"amt"},
    "op":    "gt",
    "right": {"kind":"literal","value":{"tag":"i64","value":1000}}
  },
  "returns": ["p", "med", "allergen"],
  "limit": 1000
}
```

### 4.1 Field semantics

- `as_of`: snapshot selector. Absent or `null` = engine's latest committed tx.
  `{"tx_id": N}` selects by tx_id. `{"timestamp_us": T}` selects the latest
  tx with `commit_ts ≤ T` (requires the engine to track per-tx commit timestamps;
  for v1 we land the tx_id form first and the timestamp form requires
  manifesting commit timestamps — see §6 below).
- `patterns`: list of pattern atoms; the planner picks join order.
- `kind`: `"entity"` or `"hyperedge"`. Resolver fills this in based on the
  type name's dictionary kind.
- `self_var`: optional variable that binds the entity/hyperedge UUID.
- `role_bindings`: hyperedges only; one per role with a term (var or literal).
- `property_filters`: entities and hyperedges. `op` is one of
  `eq` `ne` `lt` `le` `gt` `ge`. The RHS is a `Term`:
  - `term = Var { name }` + `op = Eq` → **bind**: variable receives the
    property value at match time. Used for `customer(name: ?n)`.
  - `term = Literal { value }` + `op = Eq` → equality **filter**. Used
    for `customer(region: "Vietnam")`.
  - `term = Literal { value }` + other op → ordered filter (clients
    may emit; the parser only produces `Eq` in v1).
- `recursion`: hyperedges only. `{"kind":"star"|"plus"|"optional"|"bounded",
  "min":N, "max":N, "max_depth":N}`. `max_depth` is the cycle-protection cap;
  defaults to 64; query may override via a future `max_depth N` clause (not in
  v1).
- `filter`: optional boolean expression tree over bound variables.
- `returns`: list of variable names to project. Each must be bound by some
  pattern's `self_var`, role binding, or property filter.
- `limit`: optional cap on result tuples.

### 4.2 Tagged-union conventions

Mirrors the existing `JsonValue` shape from `ndb-engine::wire`:

- `Pattern` is tagged on `kind` (`"entity"` | `"hyperedge"`).
- `Term` is tagged on `kind` (`"var"` | `"literal"`).
- `Expr` (filter) is tagged on `kind` (`"and"` | `"or"` | `"not"` | `"cmp"`).
- `Recursion` is tagged on `kind` (`"star"` | `"plus"` | `"optional"` | `"bounded"`).
- `AsOf` is tagged on the present key (`"tx_id"` | `"timestamp_us"`).

Tags use snake_case. All field names use snake_case.

### 4.3 Response shape

```jsonc
{
  "columns": ["p", "med", "allergen"],
  "rows": [
    [{"tag":"uuid","value":"..."}, {"tag":"uuid","value":"..."}, {"tag":"string","value":"penicillin"}],
    ...
  ],
  "truncated": false
}
```

- `columns` matches the `returns` list order.
- `rows` is an array of arrays of `JsonValue`s (the existing tagged-union value
  shape — reuse the engine's `JsonValue`).
- `truncated`: `true` if `limit` capped the result; `false` otherwise.

---

## 5. Semantics

### 5.1 Variable binding rules

- A variable is **bound** at its first occurrence as a `self_var`, a role
  binding's `term`, or a property filter's `term`.
- Subsequent occurrences must unify (must be equal to the existing binding
  in any candidate tuple, else the candidate is filtered out).
- All variables appearing in `return` and `filter` must be bound by some
  pattern in `match`. Unbound variables → semantic error at resolve time.

### 5.2 Pattern matching

For each pattern atom:

- **Entity pattern**: yields candidate tuples of `(self_var, prop_filter
  vars)`. Engine primitive used depends on what's bound — see planner §7.
- **Hyperedge pattern (non-recursive)**: yields candidates over
  `(self_var, role-binding vars, prop_filter vars)`.
- **Hyperedge pattern (recursive)**: yields candidate tuples that bind any
  variables on the recursive roles to the endpoint values reachable by the
  recursive walk. See §5.3 for recursion semantics.

Atom outputs are joined on shared variable bindings.

### 5.3 Recursive-path semantics

Closed (locked) decisions:

1. **Snapshot scope**: the entire recursive closure is evaluated at the
   query's `as_of` snapshot. The walk does NOT re-snapshot per step. This
   matches MVCC's reader-doesn't-block-writer guarantee and makes recursive
   queries trivially repeatable.

2. **Termination**: every recursive atom has a `max_depth` cap (default 64).
   The executor maintains a `visited: HashSet<EntityId>` for the frontier
   and refuses to re-add visited nodes — cycle protection is therefore
   intrinsic. If the cap is reached without exhausting the frontier, the
   query returns an error `recursion_depth_exceeded` with the depth, the
   pattern's source span, and the size of the unexpanded frontier. The
   query does NOT silently truncate.

3. **Direction**: a recursive hyperedge pattern names two roles — a "start"
   role bound to a concrete value (or a previously-bound variable), and an
   "end" role bound to a variable that captures reachable endpoints. The
   walk traverses `start → end` direction only. To traverse the inverse
   direction, swap the roles. v2 may add `bidirectional` modifier.

4. **Multiplicity**: `*` includes zero-step matches (the start node itself
   counts as a path of length 0, binding the end variable to the start
   value). `+` excludes zero-step (at least one hop). `?` is zero-or-one.
   `{n,m}` requires inclusive bounds, `n ≤ m`, `m ≤ 64`.

5. **Result shape**: a recursive pattern produces one tuple per `(start,
   end)` pair, regardless of how many paths exist between them. Path
   enumeration is not exposed in v1.

### 5.4 `as of <expr>` semantics

- `as of <integer>` selects the snapshot at that tx_id. If the tx_id has
  been compacted out, the query returns `snapshot_unavailable` with the
  oldest live tx_id.
- `as of "<rfc3339-timestamp>"` selects the latest tx with
  `commit_timestamp ≤ T`. Requires the engine to track commit timestamps —
  v1 lands the tx_id form first; timestamp form is one of the §6 follow-on
  tasks.

### 5.5 `where` semantics

- Comparison: `term op term`. If either term is unbound at the comparison
  point, semantic error. If types are incompatible (string compared to
  i64), the comparison is FALSE for that candidate (does not crash).
- Boolean composition: standard, with precedence per §3.1.
- Short-circuit evaluation is permitted but not observable (no side effects
  in v1).

### 5.6 `return` and `limit`

- `return ?a, ?b` projects each candidate tuple to the listed columns.
- `limit N` caps the result at N tuples. The planner is free to push the
  limit down past joins where correctness allows. Result is unordered for
  v1 — `limit` is "stop after N", not "top-N".

### 5.7 Hyperedge pattern semantics (locked)

The locks below resolve specific n-arity questions surfaced in the
2026-05-27 review.

**Partial role match is the default.** A pattern names ONLY the roles
the user cares about. Unnamed roles are wildcards — they may be bound
to any entity in the candidate hyperedge. A 5-arity `prescription`
hyperedge with roles `patient`, `prescriber`, `medication`, `dose`,
`frequency` is matched by:

```
prescription(patient: ?p, medication: ?m)
```

→ any prescription where `patient` and `medication` exist, regardless
of the other three role values. The planner asks the engine for
hyperedges of type `prescription`, then filters by the two named
bindings. The three unconstrained roles contribute nothing to the
filter — they do not require explicit `_` placeholders.

To require a role to be PRESENT but unconstrained (e.g. you want only
prescriptions that have a `prescriber` role bound, regardless of who),
use `_`: `prescription(patient: ?p, prescriber: _)`. v1 treats `_` as
a fresh anonymous variable that doesn't participate in joins or
returns.

**Same-hyperedge variable repetition unifies.** A variable mentioned
twice in the same pattern requires the two role bindings to be equal:

```
approval(document: ?d, approver: ?p, document_owner: ?p)
```

→ matches approvals where the approver and the document_owner role are
bound to the same entity. No join needed; unification is intrinsic.

**Role vs property name resolution (Option A — overload by name).**
The single binding-list syntax accepts both roles and properties. The
resolver decides for each name:

- Name registered as a role for this type → role binding.
- Name registered as a property key for this type → property filter.
- Name registered as BOTH a role and a property key for this type →
  resolver returns `ambiguous_name` error with the type and name. The
  fix at schema-definition time is to rename one or the other. This is
  rare; nDB's schemaless core doesn't enforce role/property name
  partitioning, but real schemas naturally separate them.
- Name registered as neither → `unknown_role_or_property` error.

This preserves the §12.6 example syntax verbatim
(`approval(document: ?doc, approver: ?alice, workflow: "fast-track")`)
without forcing users to memorise role-vs-property tax. The
ambiguity-error path catches naming collisions early.

**Hyperedge recursion across higher-arity types.** A recursive pattern
must name exactly two roles for the walk endpoints (start and end).
Other roles of the hyperedge type may also be named to constrain the
walk:

```
contains*(parent: body_42, child: ?leaf, layer: "organ")
```

→ traverses `contains` hyperedges where `layer = "organ"` AT EVERY
STEP, walking from `parent = body_42` outward. Roles not named are
wildcards per step.

**Hyperedge UUID binding.** The hyperedge record's own UUID is bound
via `as ?var`. The UUID is then a regular variable for joining,
filtering, and returning. Two hyperedges of the same type with the
same role bindings can be distinguished by their UUIDs.

**Hyperedge property filtering inside the pattern.** Properties of the
hyperedge itself (not its role-bound entities) appear in the same
binding list as roles, distinguished by the resolver per the
role-vs-property rule above. To filter by a hyperedge property
comparison (not equality), bind the hyperedge to a variable and use
`where`:

```
approval(document: ?d, approver: ?a) as ?app
where ?app != ?prev_app
```

Variable-vs-variable comparisons and inequality operators on properties
require the where-clause path — pattern-internal filters are always
equality (or, equivalently, "bind a variable and use `where`"). v2 may
add inline property comparisons (`property OP literal`).

---

## 6. Error model

Three layers. Each carries a `code`, a human-readable `detail`, and where
applicable a `span` `{line, column, length}` pointing into the source text.

### 6.1 Lexer errors

`lex_error` — unexpected character, unterminated string, malformed number,
malformed UUID literal. Always carries a `span`.

### 6.2 Parser errors

`parse_error` — unexpected token, missing keyword, malformed pattern.
Always carries a `span` pointing to the offending token, plus an `expected`
list when one-of-N would be useful (e.g. `expected: '(' or 'as'`).

### 6.3 Semantic errors (post-name-resolution)

| Code | Meaning |
|---|---|
| `unknown_type` | Type name not in dictionary |
| `ambiguous_type` | Name matches both entity and hyperedge dictionaries |
| `unknown_role` | Role name not in dictionary OR not valid for this type |
| `unknown_property` | Property name not in dictionary |
| `unknown_role_or_property` | Binding name neither a role nor a property for this type |
| `ambiguous_name` | Binding name registered as BOTH a role and a property for this type |
| `unbound_variable` | Variable in `return` or `where` not bound by any pattern |
| `type_mismatch` | Filter compares incompatible tags |
| `arity_violation` | Recursive pattern doesn't name exactly two roles |
| `recursion_bounds_invalid` | `{n,m}` with `n > m` or `m > 64` |
| `reserved_word_collision` | Dictionary entry shadows a reserved word |

### 6.4 Runtime errors (at executor time)

| Code | Meaning |
|---|---|
| `recursion_depth_exceeded` | Cap reached during traversal |
| `snapshot_unavailable` | `as_of` tx_id has been compacted out |
| `result_too_large` | Implementation-defined hard cap (default 1M rows pre-limit) reached |

Wire shape (matches existing `ErrorResponse` extended):

```jsonc
{
  "error": "parse_error",
  "detail": "expected ')' or ',', got identifier 'foo' at line 3 col 24",
  "span": { "line": 3, "column": 24, "length": 3 }
}
```

---

## 7. Planner sketch

Targets the resolved wire AST. v1 algorithm:

1. **Cardinality estimate per atom**:
   - Entity pattern with a property filter where `(type_id, property_id)` has
     a B-tree → estimate via B-tree's per-value entry count.
   - Entity pattern with no property filter → engine type-cluster count.
   - Hyperedge pattern with all-literal roles → 0 or 1 (point lookup).
   - Hyperedge pattern with one bound variable + literal roles → adjacency
     index estimate.
   - Recursive pattern → cardinality of the unbound endpoint, scaled by
     expected fanout (heuristic: average degree of the source type from
     adjacency index stats).

2. **Pick smallest-cardinality atom as the seed**. Materialize candidates
   via the matching engine primitive:
   - `lookup_by_external_key` for unique-property entity patterns.
   - `property_lookup` / `property_range` for B-tree-indexed entity patterns.
   - `hyperedges_by_type` for type-only hyperedge patterns.
   - `hyperedges_for_entity` for adjacency-walk hyperedge patterns when one
     role is bound.

3. **Greedy join order**: at each step pick the unjoined atom that shares
   the most variables with the running join, breaking ties by cardinality.
   For each candidate from the running join, look up matching atoms and
   filter on `where`.

4. **Push down `where`**: filters that touch only one atom's variables run
   at scan time on that atom. Cross-atom filters run at join time.

5. **`limit` push-down**: if the join is on a unique constraint, push limit
   into the seed scan. Otherwise apply at the top.

v2 may replace the greedy planner with cost-based DP. v1's greedy is
provably correct (only the order changes, not the result set) and fast to
build.

---

## 8. Examples

### 8.1 Basic point lookup

```
match
  customer(name: ?name, region: "Vietnam") as ?cust
return ?cust, ?name
limit 100
```

→ Resolver: customer is entity-type → property_filters on `name` and `region`.
→ Planner: if `(customer, region)` is B-tree-indexed, seed via
`property_lookup(customer_type_id, region_prop_id, "Vietnam")`; otherwise
scan + filter.

### 8.2 Joining patterns + scalar filter

```
match
  sales_order(customer: ?cust, amount: ?amt, posting_date: ?dt) as ?so
  customer(name: ?name, region: "Vietnam") as ?cust
where ?amt > 1000
return ?name, ?amt, ?dt
limit 1000
```

→ Two patterns sharing `?cust`. Resolver: `sales_order` is hyperedge-type,
`customer` is entity-type. Planner picks `customer(region="Vietnam")` as
the seed (smaller via B-tree), iterates customers, joins on the `customer`
role of each `sales_order`, applies `?amt > 1000`, projects.

### 8.3 Same-hyperedge-type co-pattern

```
match
  approval(document: ?doc, approver: ?alice) as ?a1
  approval(document: ?doc, approver: ?bob)   as ?a2
where ?alice != ?bob
return ?doc, ?alice, ?bob
```

Finds documents with at least two distinct approvers.

### 8.4 Medical diagnostic (the §12.6 motivator)

```
match
  diagnosis(patient: ?p, symptom: "fever", pathogen: ?d)
  diagnosis(patient: ?p, symptom: "rash",  pathogen: ?d)
  treatment(disease: ?d, medication: ?med, contraindication: ?a)
  patient_record(known_allergy: ?a) as ?p
return ?p, ?med, ?a
```

Note `patient_record(... ) as ?p` — the patient entity binds via self-bind,
its `known_allergy` property filters to `?a`. No more magical `id:` key.

### 8.5 Recursive transitive closure

```
match
  contains*(parent: uuid:01923c..., child: ?leaf)
  amino_acid(name: ?name) as ?leaf
return ?leaf, ?name
```

→ Resolver: `contains` is hyperedge-type with roles `parent` and `child`;
recursion marker `*` parsed; literal start (`uuid:...`); end variable
`?leaf`. Executor BFS over `contains` hyperedges, frontier from `parent =
01923c...` to `child = ?`, dedup on visited, cap at 64 depth. Joins with
`amino_acid` entity pattern via shared `?leaf`.

### 8.6 Time travel

```
as of 1700000000000042
match
  customer(name: ?name) as ?cust
return ?cust, ?name
limit 50
```

→ Engine snapshot pinned to tx_id 1700000000000042. Same query, different
snapshot, different results — but always deterministic given the tx_id.

---

## 9. Out-of-scope and rejected ideas (record for v2)

- **Aggregation in queries** — lives in slicer (§7 of parent spec); engine
  query language stays minimal. Returning a result set + piping to slicer
  is the path.
- **SQL `ORDER BY`** — slicer concern. Returning an unordered set + sorting
  client-side is the path. v2 may add for the cases where the engine has a
  sorted index it can scan in order (small efficiency win, not required).
- **JOIN keyword** — joins are implicit on shared variables. No `JOIN`
  syntax in v1; it would be redundant.
- **CTEs and subqueries** — v2. The current scope of v1 queries is
  expressible without them.
- **Index hints / planner directives** — v2. Planner picks; observed in
  benchmarks.
- **Writes via query** — use `/commit`. Allowing writes through query
  syntax would force read-set tracking + conflict detection per query and
  complicate the executor; not worth it for v1.
- **Per-step snapshot for recursion** — rejected. Single-snapshot semantics
  is simpler and lines up with the rest of MVCC.
- **`a < b < c` chained comparisons** — non-associative; force `and`.
- **Quoted identifiers (`` `where` ``)** — v2 if dictionary collisions
  with reserved words become a real problem; v1 forbids the collision at
  resolve time.

---

## 10. Build plan (drives the next several commits)

| Step | Crate / module | Tests landed first |
|---|---|---|
| 1 | `ndb-engine::wire::QueryRequest` / `Response` + tagged-union AST + serde | round-trip per atom kind |
| 2 | `ndb-query` crate: lexer (tokens + spans) | per-token unit tests |
| 3 | `ndb-query` crate: parser (one-token lookahead, recursive descent) | per-grammar-rule unit tests; parser-output equals expected AST |
| 4 | `ndb-query` crate: error rendering | each error kind has a test |
| 5 | `ndb-engine` resolver: name AST → wire AST via dictionaries | name lookup, ambiguous-type, unknown-role |
| 6 | `ndb-engine` planner: AST → plan tree | per-pattern test asserting chosen primitive |
| 7 | `ndb-engine` executor: plan tree → result rows | per-example query test against in-process engine |
| 8 | `ndb-server::handle_query` route | TCP round-trip test for each §8 example |
| 9 | `ndb-client-rust::Client::query` + Python `client.query` + CLI `ndb query` | client surface tests |
| 10 | Recursive-pattern executor (BFS with depth cap + visited set) | cycle test, depth-cap test, star/plus/optional/bounded |
| 11 | `as_of` integration (tx_id form first; timestamp form follows once commit timestamps land) | snapshot-isolation test |

Each step ships as its own commit with tests green and clippy clean.

---

## 11. Open items intentionally NOT closed by this spec

These belong to follow-on work and don't gate the v1 query-language release:

- Cost-based planner (currently greedy)
- Index hints / `/*+ ... */` directives
- Per-tx commit-timestamp tracking required for `as of "<rfc3339>"` form
- Result streaming (chunked transfer) — handled by the separate streaming-cursor
  workitem (§17.1 deliverable #5)
- Subqueries / CTEs
- Aggregation in-query (will not happen; slicer is the path)

Once v1 query language lands and runs on real workloads, a v2 working spec
will revisit these in priority order.
