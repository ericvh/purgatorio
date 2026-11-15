//! p9sk1 against Plan 9's own code: `PURG_P9SK1_VECTORS` names a file of
//! vectors that `.github/interop/p9sk1vec.c`, built with plan9port's
//! libauthsrv and libsec, printed (the nightly interop workflow). Without
//! it the test says so and passes.

use purg_auth::hex;
use purg_auth::p9sk1::{self, AUTH_AC, AUTH_TREQ, AUTH_TS, Authenticator, Ticket, TicketReq};

fn bytes(h: &str) -> Vec<u8> {
    hex::decode(h.as_bytes()).expect("hex")
}

#[test]
fn p9sk1_matches_plan9() {
    let Some(path) = std::env::var_os("PURG_P9SK1_VECTORS") else {
        eprintln!("plan9_vectors: skipped: PURG_P9SK1_VECTORS is not set");
        return;
    };
    let text = std::fs::read_to_string(path).expect("vectors");
    let key = p9sk1::passtokey("secret");
    let mut seen = 0;
    for line in text.lines() {
        let w: Vec<&str> = line.split(' ').collect();
        match w.as_slice() {
            ["passtokey", pw, h] => {
                assert_eq!(p9sk1::passtokey(pw).to_vec(), bytes(h), "passtokey {pw}");
            }
            ["encrypt", n, h] => {
                let n: usize = n.parse().unwrap();
                let mut b: Vec<u8> = (0..n as u8).collect();
                assert!(p9sk1::encrypt(&key, &mut b));
                assert_eq!(b, bytes(h), "encrypt {n}");
                assert!(p9sk1::decrypt(&key, &mut b));
                assert_eq!(b, (0..n as u8).collect::<Vec<u8>>(), "decrypt {n}");
            }
            ["ticket", h] => {
                let t = Ticket {
                    num: AUTH_TS,
                    chal: [3; 8],
                    cuid: "glenda".into(),
                    suid: "bootes".into(),
                    key: [5; 7],
                };
                assert_eq!(t.encode(&key).to_vec(), bytes(h), "ticket");
            }
            ["authenticator", h] => {
                let a = Authenticator {
                    num: AUTH_AC,
                    chal: [7; 8],
                    id: 0x01020304,
                };
                assert_eq!(a.encode(&key).to_vec(), bytes(h), "authenticator");
            }
            ["ticketreq", h] => {
                let r = TicketReq {
                    kind: AUTH_TREQ,
                    authid: "bootes".into(),
                    authdom: "example.com".into(),
                    chal: [9; 8],
                    hostid: "glenda".into(),
                    uid: "glenda".into(),
                };
                assert_eq!(r.encode().to_vec(), bytes(h), "ticketreq");
            }
            _ => panic!("unknown vector line: {line}"),
        }
        seen += 1;
    }
    assert!(seen >= 20, "only {seen} vectors");
}
