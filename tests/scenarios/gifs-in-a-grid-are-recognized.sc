# GIFs in a grid are GIFs like a lone one: marked, and playing in place, one at a time.
settle
fx seed who=1 n=3
settle
open 1
settle
fx gifs who=1 n=2
settle
settle
expect ui cells = 2
expect ui gif-cells = 2
expect ui blank-cells = 0
expect ui playing-cells = 0
# the pointer on a cell plays that cell only, and leaving it stops it
move 390 300
wait 800
expect ui playing-cells = 1
move 540 300
wait 800
expect ui playing-cells = 1
move 150 400
wait 800
expect ui playing-cells = 0
