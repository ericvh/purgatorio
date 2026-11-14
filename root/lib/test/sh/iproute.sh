# /net/iproute (ip(3)): add, remove, tag and flush, and reads listing
# target, mask, gateway, type, tag and interface. Only routes into
# 198.18.0.0/15, a benchmarking range, are shown: the interfaces' own
# routes and the default route differ between the emu and the kernel, so
# the script never flushes every route either.
load std
fn routes {
	grep '^198\.1[89]\.' /net/iproute
}
echo add 198.18.1.0 255.255.255.0 198.19.0.1 > /net/iproute
# A tag holds for the descriptor it was written on; a target is masked.
{
	echo tag test
	echo add 198.18.2.7 /120 198.19.0.2
	echo add 198.18.3.0 255.255.255.128 198.19.0.3
} > /net/iproute
routes
echo again
# Adding a route again changes its gateway and keeps its tag.
echo add 198.18.2.0 255.255.255.0 198.19.0.9 > /net/iproute
routes
echo remove
# remove takes one route; one that is not there is no error.
echo remove 198.18.1.0 255.255.255.0 > /net/iproute
echo remove 198.18.1.0 /120 > /net/iproute
routes
echo flush
# flush with a tag takes only the routes with that tag.
echo flush test > /net/iproute
routes
echo gone
# Too few fields and bad addresses are refused; a verb iproute does not
# know is ignored, as in Inferno.
{
	echo add 198.18.1.0 255.255.255.0 > /net/iproute
	echo remove 198.18.1.0 > /net/iproute
	echo add 198.18.1 255.255.255.0 198.19.0.1 > /net/iproute
	echo add 198.18.1.0 /24 198.19.0.1 > /net/iproute
	echo tag > /net/iproute
	echo frob 198.18.1.0 > /net/iproute
} >[2=1]
routes || echo still gone
