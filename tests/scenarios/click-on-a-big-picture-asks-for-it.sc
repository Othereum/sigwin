# A picture past the size that is downloaded by itself waits, showing its blurhash. A click on it
# asks for it, and the click gets through.
settle
fx seed who=1 n=3
settle
open 1
settle
fx big who=1
settle
settle
expect ui cells = 1
expect backend allowed = 0
click 400 300
settle
expect backend allowed = 1
