# The @mention popup offers every member, scrolling past the rows it shows.
settle
fx seed who=1 name=철수 n=3
settle
open 1
settle
members Ann|Ben|Cal|Dee|Eve|Fay|Gus|Hal|Ivy|Jon
click 600 645
type @
expect ui mention-rows = 10
