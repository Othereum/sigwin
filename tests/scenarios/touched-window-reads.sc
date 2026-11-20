# A window without the focus that merely shows reads nothing. Moving the mouse over it (a second
# monitor) reads what the list shows, and news while the mouse is at it stays at the bottom and
# is read. A few seconds after the last move, or once the mouse leaves, nothing is read again.
settle
fx seed who=1 name=철수 n=100
settle
open 1
settle
focus off
settle
expect ui inactive = true
fx recv who=1 n=2
settle
expect backend unread who=1 = 2
expect backend sent who=1 = delivery=2 read=0 sync=0
# the mouse moves over it: the rows on screen are read
touch on
settle
expect ui inactive = false
expect backend unread who=1 = 0
expect backend sent who=1 = delivery=2 read=2 sync=2
# news while the mouse is at it
fx recv who=1 n=2
settle
expect ui bottom-gap = 0
expect backend unread who=1 = 0
expect backend sent who=1 = delivery=4 read=4 sync=4
# no move for a while: back to the background
wait 3500
settle
expect ui inactive = true
fx recv who=1 n=2
settle
expect backend unread who=1 = 2
# the mouse leaves at once
touch on
settle
touch off
settle
expect ui inactive = true
fx recv who=1 n=2
settle
expect backend unread who=1 = 2
