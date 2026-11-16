# The composer shrinking back (a second line deleted) while the list is at the bottom keeps the
# newest message in view: the list re-seeks past its end, and the rows must follow, not stay off
# screen (blank list).
settle
fx seed who=1 name=철수 n=100
settle
open 1
settle
click 600 645
type abc
key shift+Enter
type d
settle
expect ui bottom-gap = 0
key Backspace
key Backspace
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
