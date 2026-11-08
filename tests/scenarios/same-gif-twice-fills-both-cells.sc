# The same GIF sent twice in one message: both grid cells have one key, and the picture arrives
# after the rows. Both cells must get it, not only the first.
settle
fx seed who=1 n=3
settle
open 1
settle
fx gifs who=1 n=2 same=1
settle
settle
expect ui cells = 2
expect ui blank-cells = 0
