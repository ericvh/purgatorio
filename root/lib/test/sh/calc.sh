# calc: expressions, statements, functions and the built-in forms.
d=/tmp/sh-calc
rm -rf $d
mkdir -p $d
calc '1 + 2 * 3'
calc '2 ** 10; 7 // 2; 7 % 3; 5!; -2 ** 2'
calc '0x1f + 16r10 + 017'
echo 'x = 3
x * x
x += 1; x
y := 2; x * y
1 < 2 && 2 < 1
1 ? 5 : 6
0 -> 1; 1 <-> 0
6 & 3; 6 | 3; 6 ^ 3; 1 << 4
sqrt 16 + 1
max(3, 9, 4); min(3, 9, 4); abs(-2); floor(2.5); round(2.5); sign(-3)
1/3
1/0
sigma(i = 0, 100, 1/i!)
pi(i = 1, 5, i)
differential(x = 1, x*x+5*x+6)
printbase = 2; 10; printbase = 10
' | calc
echo 'fn ack(a, b)
{
	n = n+1
	if(a == 0)
		return b+1;
	if(b == 0)
		return ack(a-1, 1);
	return ack(a-1, ack(a, b-1));
}
for(i = 0; i < 2; i++)
	for(j = 0; j < 3; j++){
		n = 0
		print "ack(", i, ",", j, ")=", ack(i, j), " in ", n, " calls\n"
	}
x = 10
if (x > 5) print "big\n" else print "small\n"
do { x-- } while (x > 7)
x
while (1) { x++; if (x > 9) break }
x
s = 0
for (i = 1; i <= 10; i++) { if (i % 2) continue; s += i }
s
' > $d/prog
calc $d/prog
echo 'solve(x**2 - 5*x + 6 == 0)' | calc
echo 'print "before\n"
1 +
nosuch(1)
exit
print "after\n"' | calc
calc -s 'q + 1'
calc -s 'q := 4; q + 1'
rm -r $d
