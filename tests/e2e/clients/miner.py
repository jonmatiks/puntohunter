"""A deterministic V1 share generator with a controllable hit rate.

It grinds nonces in pure Python. With the harness's vardiff floor below regtest's network difficulty, a share
takes a few hashes, and about half of all shares would be blocks. A share is therefore a block only when asked
for one, so that tests submitting ordinary shares never move the chain; `meets=False` finds a hash that misses
the target instead, for low-difficulty tests.
"""

import hashlib
import itertools
from fractions import Fraction

DIFF1 = 0xFFFF << 208
BIP320_MASK = 0x1FFFE000


def target(difficulty):
    """Stratum's target for `difficulty`: diff1 / difficulty, saturating at 2^256 - 1."""
    return min(int(Fraction(DIFF1) / Fraction(difficulty)), 2**256 - 1)


def network_target(nbits):
    """The block target of a notify's nbits."""
    bits = int(nbits, 16)
    return (bits & 0xFFFFFF) << (8 * ((bits >> 24) - 3))


def sha256d(data):
    return hashlib.sha256(hashlib.sha256(data).digest()).digest()


def header(notify, extranonce, ntime, nonce, version_bits=0, mask=BIP320_MASK):
    """The 80-byte header a miner builds from mining.notify params."""
    root = sha256d(bytes.fromhex(notify[2]) + extranonce + bytes.fromhex(notify[3]))
    for sibling in notify[4]:
        root = sha256d(root + bytes.fromhex(sibling))
    prevhash = bytes.fromhex(notify[1])
    prevhash = b"".join(prevhash[i : i + 4][::-1] for i in range(0, 32, 4))
    version = (int(notify[5], 16) & ~mask) | (version_bits & mask)
    return (
        version.to_bytes(4, "little")
        + prevhash
        + root
        + ntime.to_bytes(4, "little")
        + int(notify[6], 16).to_bytes(4, "little")
        + nonce.to_bytes(4, "little")
    )


class ShareGenerator:
    def __init__(self, extranonce1, extranonce2_size):
        self.extranonce1 = bytes.fromhex(extranonce1)
        self.extranonce2_size = extranonce2_size
        # A fresh extranonce2 per share, so no two shares repeat a header.
        self.counter = itertools.count()

    def share(self, notify, difficulty, meets=True, version_bits=0):
        """Submit params after the username for a share on `notify` that meets `difficulty`'s target and is
        not a block, or that misses the target."""
        goal, block = target(difficulty), network_target(notify[6])
        if meets:
            assert goal > block, f"every share at difficulty {difficulty} is a block"
            return self._grind(notify, block, goal, version_bits)[0]
        return self._grind(notify, goal, 2**256, version_bits)[0]

    def block(self, notify, difficulty, version_bits=0):
        """A share on `notify` that meets `difficulty`'s target and is also a block: its submit params, and
        the block's hash in display order."""
        goal, block = target(difficulty), network_target(notify[6])
        return self._grind(notify, -1, min(goal, block), version_bits)

    def _grind(self, notify, above, up_to, version_bits):
        """The params and hash of the first header whose hash value v has above < v <= up_to."""
        extranonce2 = next(self.counter).to_bytes(self.extranonce2_size, "big")
        ntime = int(notify[7], 16)
        for nonce in range(2**32):
            data = header(notify, self.extranonce1 + extranonce2, ntime, nonce, version_bits)
            digest = sha256d(data)
            if above < int.from_bytes(digest, "little") <= up_to:
                params = [notify[0], extranonce2.hex(), f"{ntime:08x}", f"{nonce:08x}"]
                if version_bits:
                    params.append(f"{version_bits:08x}")
                return params, digest[::-1].hex()
        raise AssertionError("no nonce gives the wanted result")
