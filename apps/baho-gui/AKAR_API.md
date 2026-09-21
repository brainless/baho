# Pinned Akar API contract

`baho-gui` pins all Akar crates to commit
`16755a8c1d9a66e7c3346ae5602167f7cf7cda78`. The local checkout at
`/Users/brainless/Projects/akar` was verified at that revision before Epic 005
UI work.

The prompt UI may rely on these synchronous, caller-owned contracts:

- `text_input` accepts `&mut String` and `&mut TextEditState`, returns
  `TextInputResponse`, and owns focus through `core.input.focused_id` using
  `layout.widget_id(node_id)`.
- `button` returns `ButtonResult`; `clicked` is the one-frame submit edge.
- `data_grid_begin` accepts caller-owned `DataGridState` and stable row and
  column keys. Its response exposes visible row and column ranges for bounded
  cell submission.
- `data_grid_handle_keyboard` processes navigation only when
  `core.input.focused_id == Some(layout.widget_id_keyed(node, 0))`. Prompt and
  grid keyboard handling can therefore be separated by widget focus.
- The grid lifecycle remains `data_grid_begin`, header begin/cells/end, body
  begin/cells/end, then `data_grid_end`.

These APIs were checked in the pinned versions of `text_input.rs`, `button.rs`,
`data_grid.rs`, `input.rs`, and the data-grid/demo examples. Akar remains
synchronous and does not own the application event loop.
