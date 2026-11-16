# @mention candidates follow a syllable an IME is still composing, not only what it committed:
# with "이" composing after "@", the two 이s are offered; with "이서", only 이서연.
settle
fx seed who=1 name=철수 n=3
settle
open 1
settle
members 이서연|이민준|박지훈
click 600 645
type @
expect ui mention-rows = 3
ime 이
expect ui mention-rows = 2
commit 이
ime 서
expect ui mention-rows = 1
commit 서
expect ui mention-rows = 1
