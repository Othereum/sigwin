# While the user is away nothing is read; coming back reads what is on screen.
away on
settle
fx seed who=1 n=100 unread=30
settle
open 1
settle
expect ui inactive = true
expect ui open-unread = 30
expect backend unread who=1 = 30
expect backend sent who=1 = delivery=0 read=0 sync=0
away off
settle
expect ui inactive = false
expect ui open-unread < 30
