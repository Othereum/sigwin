# More unread messages than the window holds: the thread opens at the first one, and the
# button brings the newest in and lands at the bottom, reading the rest.
settle
fx seed who=1 n=300 unread=150
settle
open 1
settle
expect ui near-bottom = false
expect ui open-unread > 100
jump
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
expect ui open-unread = 0
expect backend unread who=1 = 0
expect backend sent who=1 = delivery=0 read=150 sync=150
