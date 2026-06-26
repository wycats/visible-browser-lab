# Agent Surface Evaluation

- Model: `gpt-5.5`
- Reasoning effort: `medium`
- Successful tasks: `29/30` (96.7%)
- Correct first selections: `29/30` (96.7%)
- Semantic fallback violations: `0`
- Unowned backend actions: `0`
- Acceptance: `passed`

## Categories

| Category | Successful | Total |
| --- | ---: | ---: |
| diagnostics | 3 | 4 |
| dialogs_files | 3 | 3 |
| emulation | 2 | 2 |
| forms | 4 | 4 |
| frames | 2 | 2 |
| history | 3 | 3 |
| ownership | 2 | 2 |
| page_discovery | 2 | 2 |
| performance | 2 | 2 |
| specialized | 3 | 3 |
| waits | 3 | 3 |

## Trials

| Fixture | Tools | Success | First selection |
| --- | --- | --- | --- |
| page-discovery-heading-navigation | start_session -> snapshot -> close_tab | true | true |
| page-discovery-live-region | start_session -> snapshot -> close_tab | true | true |
| form-fill-single | start_session -> snapshot -> fill -> close_tab | true | true |
| form-fill-multiple | start_session -> snapshot -> fill_form -> close_tab | true | true |
| form-select-checkbox | start_session -> snapshot -> help:fill_form -> fill_form -> close_tab | true | true |
| form-contenteditable | start_session -> snapshot -> type_text -> close_tab | true | true |
| wait-text-appearance | start_session -> snapshot -> wait_for -> close_tab | true | true |
| wait-element-disappearance | start_session -> snapshot -> wait_for -> close_tab | true | true |
| wait-url-transition | start_session -> wait_for -> close_tab | true | true |
| navigate-url | start_session -> navigate -> close_tab | true | true |
| navigate-history-back | start_session -> navigate -> close_tab | true | true |
| navigate-reload | start_session -> navigate -> close_tab | true | true |
| frame-fill | start_session -> snapshot -> fill -> snapshot -> close_tab | true | true |
| nested-frame-click | start_session -> snapshot -> snapshot -> snapshot -> click -> close_tab | true | true |
| dialog-confirm | start_session -> snapshot -> click -> interact:handle_dialog -> snapshot -> close_tab | true | false |
| file-upload | start_session -> snapshot -> help:upload_files -> interact:upload_files -> snapshot -> close_tab | true | true |
| file-drop | start_session -> snapshot -> help:drop -> interact:drop -> snapshot -> close_tab | true | true |
| console-error-diagnosis | start_session -> snapshot -> console -> console:list -> console:get -> close_tab | true | true |
| console-message-detail | start_session -> console:get -> close_tab | true | true |
| network-failed-request | start_session -> help:network -> network -> network:list -> network:get -> close_tab | false | true |
| network-response-body | start_session -> network -> network:get -> close_tab | true | true |
| performance-vitals | start_session -> performance -> performance:vitals -> close_tab | true | true |
| performance-trace-analysis | start_session -> performance:start_trace -> performance:stop_trace -> performance:analyze -> close_tab | true | true |
| emulation-mobile-viewport | start_session -> help:set_viewport -> emulation:set_viewport -> close_tab | true | true |
| emulation-offline-reset | start_session -> help:set_network -> emulation:set_network -> emulation:set_network -> emulation:reset -> close_tab | true | true |
| ownership-foreign-action-refusal | start_session -> list_tabs -> click -> close_tab | true | true |
| ownership-readonly-inventory | start_session -> list_tabs -> close_tab | true | true |
| audit-accessibility | start_session -> audit -> audit:run -> close_tab | true | true |
| memory-retaining-path | start_session -> memory:capture -> help:retaining_paths -> memory:retaining_paths -> close_tab | true | true |
| screencast-artifact-export | start_session -> screencast:start -> screencast:stop -> artifacts:export -> close_tab | true | true |
