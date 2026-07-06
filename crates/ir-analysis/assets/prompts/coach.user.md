Review the lead-up to this CS2 event and coach the player on aim and movement.

Event: {{event_kind}} at t={{event_at}}s (clip-relative).

Context (timeline markers, settings, and any computer-vision measurements):
```json
{{context_json}}
```

Frames extracted from the clip (JPEG files in this directory — read each one):
{{frame_manifest}}

Tasks, in order:
1. Read every frame image listed above.
2. Decide `eventConfirmed`: did you visually verify the event (kill feed top-right, health changes, death state)? Be strict — if the frames don't show it, say false.
3. Judge the lead-up and produce findings. Each finding has:
   - `kind`: prefer these well-known kinds where they fit — `crosshair_low`, `crosshair_off_angle`, `moving_while_shooting`, `counter_strafe_late`, `fired_before_settled`, `flick_overshoot`, `spray_too_long`, `overexposed_after_damage`, `died_flashed`, `good_counter_strafe`, `clean_flick` — or a short snake_case name for anything else.
   - `severity`: `major` (cost the fight), `minor` (suboptimal), `info` (context), `positive` (done well — include these!).
   - `confidence` 0–1: how clearly the frames support it. If the frames don't visibly support a candidate, drop it or mark it low-confidence — rejecting is a valid outcome.
   - `startS`/`endS`: the clip time range it covers, and `evidence`: the specific frame timestamps that show it, with a short note each.
   - `coaching`: 1–3 sentences, drillable, addressed to the player.
4. Write `summary`: 2–4 sentences — the single most impactful thing to work on, plus what was done well.

{{output_instructions}}
