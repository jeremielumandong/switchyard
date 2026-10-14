The physical-code table is the complete original Chromium152.0.7977.82
`ui/events/keycodes/dom/dom_code_data.inc`, retrieved from the Chromium authors:
https://github.com/chromium/chromium/blob/152.0.7977.82/ui/events/keycodes/dom/dom_code_data.inc
SHA256841e2c58e6388f14736717eb6e365c8b8e9767788e36339bf1d35a37ab48e5b2.
Original BSD license is retained in LICENSE.chromium, SHA256
368cca1106be99d39ecd32a38d8305585d802a475effb66380b91ffc9bcf709b.
This mapping source is not a frozen Electron engine or physical input acceptance.

codes.rs transcribes the first nonzero XKB native-code match, including null
DOM-code entries. It contains225 native rows/181 named DOM codes. The independent
native unit parser compares every retained original row with the generated
mapping, ignoring commented-out rows and preserving first-match behavior.
Unknown codes remain None. Physical identity never follows a keysym, displayed
key, active layout or logical GPUI keybinding.

The Source terminal bridge also follows Chromium's original
ui/events/event.cc KeyEvent::NormalizeFlags for original Control/Shift/Alt
keysyms: the press includes and the release removes that modifier's bit.
https://github.com/chromium/chromium/blob/152.0.7977.82/ui/events/event.cc
This does not infer a modifier from the physical position: a remapped key keeps
its original keysym. Other event modifiers retain their captured XKB state.
