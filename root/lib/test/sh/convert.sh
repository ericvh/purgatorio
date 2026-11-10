# Conversions: tcs between character sets, units between units.
d=/tmp/sh-convert
rm -rf $d
mkdir -p $d
tcs -l
tcs -l latin1
tcs -l -v utf16
echo -n héllo | tcs -t latin1 > $d/latin1
xd $d/latin1
tcs -f 8859-1 $d/latin1
echo
echo -n hé☺ | tcs -t ascii
echo
echo -n hé | tcs -t utf-16le | xd
echo -n hé | tcs -t utf16 | tcs -f utf16
echo
tcs -f foo < /dev/null
echo $status
tcs $d/nonexistent
echo 'inch
cm
15 pounds force/in²
atm
2 hours
minutes
mile/hr
km/hr
kg
m
nonsense
day

1|2 kg
grams' | units
units -v | grep '^inch	'
units $d/nonexistent
rm -r $d
