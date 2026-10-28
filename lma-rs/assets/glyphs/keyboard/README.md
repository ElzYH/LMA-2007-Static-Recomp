# Keyboard glyphs (#694 step d)

The 360 button glyphs replaced by the keys lma-app maps to those buttons (`lma-app/src/keys.rs`),
keyed by the hash of the guest bytes of the texture they replace (`LMA_TEX_LOG`), 64x64 RGBA: drawn by
`tools/gen_glyphs.py --set keyboard` (no copied artwork). lma-app installs them while the keyboard is the
last device used.

| file | replaces |
|---|---|
| 340157ca1854882d.png | A -> Enter |
| 56487c0a5b537226.png | B -> Esc |
| e3eefc792d9f2c5e.png | Y -> R |
| a9a23b02633dce43.png | X -> F |
| e20a3321227cad03.png | LB -> Q |
| 3b4e1b35d87a5fbf.png | RB -> E |
| 3a052683c7fe182c.png | LT -> 1 |
| 1e35308cbd0fcd67.png | RT -> 3 |
| 998de0650aab198b.png | RS -> C |
| 475d718f6a1fcd31.png | d-pad up -> Up |
| 4f0adac5b2eed887.png | d-pad down -> Down |
| 01d740ffcbd17d3e.png | d-pad left -> Left |
| d9eb1a54fb089eee.png | d-pad right -> Right |
