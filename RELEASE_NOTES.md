# 0.12.15

Requires API server 0.1.2 deployed first; update the streaming host to 0.9.16.

- Use the registration snapshot for punch keys and candidates (two signaling
  requests instead of four).
- Skip TCP dialing for known API UDP endpoints; retain direct TCP for ordinary
  saved addresses and LAN beacons.
- Serialize tunnel attempts so cancelling and reconnecting cannot leave two
  readers on the same UDP socket. Serialize STUN warm-up/cache refresh too.
- Renew active desktop/core API leases so a streaming client does not expire
  after two minutes while idle discovery remains read-only.
- Bound signaling requests to five seconds overall and remove the shared core's
  fixed 3.25-second host synchronization delay.

Regression checks cover snapshot-derived session crypto, first-attempt request
count, reconnect cancellation and exclusive socket ownership. Real macOS client
playback and cross-network NAT behavior require a live streaming test.
