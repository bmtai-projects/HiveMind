---
id: frontend-design
name: Frontend Design
description: Builds interfaces with a real visual point of view -- deliberate type, color, and layout instead of framework defaults.
---
This task is user-facing interface work. Design it, do not just wire it up.

- Decide the visual direction before writing markup: name the palette (4-6
  concrete colors), the type pairing (a display face and a body face, with
  real fallback stacks), the spacing scale, and the one element the page
  will be remembered by. State those choices in a sentence or two, then
  build to them. A design assembled component-by-component without a
  direction reads as generic no matter how clean the code is.
- Reject the defaults you would reach for on any other project. Untuned
  Bootstrap/Tailwind palettes, a lone indigo-on-white accent, evenly spaced
  cards of identical weight, and 16px-everything type are what "no decision
  was made" looks like. Pick values that suit this specific subject.
- Establish hierarchy with size, weight, and space -- not borders on
  everything. One clear focal point per screen; secondary content visibly
  secondary.
- Spend boldness in one place. A single strong signature element with
  disciplined, quiet surroundings beats five competing effects.
- Match effort to the direction: minimal work needs precise spacing and
  type; maximalist work needs elaborate execution. Elegance is executing
  the chosen direction well, not adding more.

Non-negotiable quality floor, whatever the direction:

- Responsive to mobile widths. Relative units, flexbox/grid, images capped
  at `max-width: 100%`. The body must never scroll horizontally; wide
  tables and code blocks scroll inside their own container.
- Keyboard focus stays visible, interactive targets are large enough to
  hit, text holds contrast against its background, and motion respects
  `prefers-reduced-motion`.
- Semantic elements over `div` soup: real buttons, labelled inputs,
  headings in order.

Write the copy as design material, not filler. Label things by what the
user controls, keep the same verb through a flow ("Publish" produces
"Published"), and make empty and error states say what to do next rather
than apologize. Never ship lorem ipsum.

Verify visually, not just structurally: run the thing, open it, and check
the rendered result at both a narrow and a wide width before reporting
done. A component that compiles is not a component that looks right.
