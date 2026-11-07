# Quoting, concatenation, comments and lists.
echo hello world	# a comment
echo 'single quoted' 'it''s' '' end
echo a^b 'c d'^e
x=(1 2 3)
echo $x $#x
echo $"x
echo pre^$x
echo $x^(a b c)
y=()
echo $#y
z=one
echo $z^.c
echo (a b) c
echo (a b)^.c (x y)^(1 2)
