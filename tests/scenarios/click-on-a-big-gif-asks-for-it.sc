# A GIF that isn't here can't play: the pointer resting on it must not start a playback that ends
# at once and draws the row anew, again and again, which made the mark flicker and lost the click.
settle
fx seed who=1 n=3
settle
open 1
settle
fx big who=1 gif=1
settle
settle
expect ui cells = 1
expect backend allowed = 0
move 400 300
wait 300
expect ui playing-cells = 0
click 400 300
settle
expect backend allowed = 1
