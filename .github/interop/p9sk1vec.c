/*
 * Prints p9sk1 test vectors from Plan 9's own libauthsrv and libsec, as
 * plan9port builds them, for crates/purg-auth/tests/plan9_vectors.rs
 * (the nightly interop workflow). Each line is a name, its input where
 * there is one, and the bytes in hex.
 */
#include <u.h>
#include <libc.h>
#include <authsrv.h>

static void
hex(uchar *p, int n)
{
	int i;

	for(i = 0; i < n; i++)
		print("%02x", p[i]);
	print("\n");
}

void
main(int argc, char **argv)
{
	char *pw[] = {
		"a", "secret", "12345678", "123456789", "password",
		"glendaslongpasswordthatgoespast27bytes", nil
	};
	char key[DESKEYLEN], buf[TICKREQLEN];
	uchar b[32];
	Ticket t;
	Authenticator a;
	Ticketreq tr;
	int i, n;

	USED(argc);
	USED(argv);
	for(i = 0; pw[i] != nil; i++){
		passtokey(key, pw[i]);
		print("passtokey %s ", pw[i]);
		hex((uchar*)key, DESKEYLEN);
	}
	passtokey(key, "secret");
	for(n = 8; n <= 24; n++){
		for(i = 0; i < n; i++)
			b[i] = i;
		encrypt(key, b, n);
		print("encrypt %d ", n);
		hex(b, n);
	}

	memset(&t, 0, sizeof t);
	t.num = AuthTs;
	memset(t.chal, 3, CHALLEN);
	strcpy(t.cuid, "glenda");
	strcpy(t.suid, "bootes");
	memset(t.key, 5, DESKEYLEN);
	convT2M(&t, buf, key);
	print("ticket ");
	hex((uchar*)buf, TICKETLEN);

	memset(&a, 0, sizeof a);
	a.num = AuthAc;
	memset(a.chal, 7, CHALLEN);
	a.id = 0x01020304;
	convA2M(&a, buf, key);
	print("authenticator ");
	hex((uchar*)buf, AUTHENTLEN);

	memset(&tr, 0, sizeof tr);
	tr.type = AuthTreq;
	strcpy(tr.authid, "bootes");
	strcpy(tr.authdom, "example.com");
	memset(tr.chal, 9, CHALLEN);
	strcpy(tr.hostid, "glenda");
	strcpy(tr.uid, "glenda");
	convTR2M(&tr, buf);
	print("ticketreq ");
	hex((uchar*)buf, TICKREQLEN);
	exits(nil);
}
