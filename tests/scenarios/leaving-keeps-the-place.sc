# A thread left in the middle of its history opens where it was left; one left at the bottom, at the bottom.
settle
fx seed who=1 n=200
fx seed who=2 n=200
settle
open 1
settle
wheel 650 350 5
settle
wheel 650 350 5
settle
expect ui near-bottom = false
expect ui top-off = -25
expect ui bottom-gap = 585
open 2
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
open 1
settle
expect ui near-bottom = false
expect ui top-off = -25
expect ui bottom-gap = 585
# back at the bottom, it is forgotten
wheel 650 350 -40
settle
expect ui near-bottom = true
open 2
settle
open 1
settle
expect ui near-bottom = true
expect ui bottom-gap = 0
