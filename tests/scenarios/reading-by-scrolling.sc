# Rows are read as they come fully on screen, and all of them once the bottom is reached.
settle
fx seed who=1 n=100 unread=30
settle
open 1
settle
expect ui open-unread < 30
wheel 650 350 -2
settle
expect ui open-unread < 17
wheel 650 350 -30
settle
expect ui near-bottom = true
expect ui open-unread = 0
expect backend unread who=1 = 0
expect backend sent who=1 = delivery=0 read=30 sync=30
