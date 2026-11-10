# Tracing on wasmi and at the end of a run (issue #112).
# A traced prog takes two frames a call, so it must be given room to
# recurse as deep as an untraced one.
test/debug deep 900
echo untraced $status
trace test/debug deep 900 >[2] /dev/null
echo traced $status
# A traced prog that traps stays broken. It is left behind here: the run
# must still end cleanly, with the broken prog removed, not counted as
# one that would not die.
trace test/debug trap >[2] /dev/null
echo $status | sed 's/trap: .*unreachable.*/trap: unreachable/'
echo broken `{ps | grep -c ' broken +debug$'}
