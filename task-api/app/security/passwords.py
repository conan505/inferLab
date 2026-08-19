"""Password hashing.

bcrypt with a configurable work factor, used directly rather than through
``passlib`` (which is unmaintained and currently mis-detects bcrypt 4+).
"""

import secrets

import bcrypt

#: bcrypt only considers the first 72 bytes of a password and, since 4.1,
#: refuses longer input outright. The request schema enforces the same limit, so
#: an over-long password is a clean 400 rather than a 500 from this module.
MAX_PASSWORD_BYTES = 72


class PasswordHasher:
    def __init__(self, rounds: int = 12) -> None:
        self._rounds = rounds
        # A hash of a random value, compared against when the email is unknown so
        # that login costs the same either way. Without it, response latency
        # alone would reveal which addresses have accounts.
        self._decoy_hash = bcrypt.hashpw(
            secrets.token_bytes(32).hex().encode("utf-8"), bcrypt.gensalt(rounds)
        )

    def hash(self, plaintext: str) -> str:
        encoded = plaintext.encode("utf-8")
        if len(encoded) > MAX_PASSWORD_BYTES:
            raise ValueError(f"password exceeds {MAX_PASSWORD_BYTES} bytes")
        return bcrypt.hashpw(encoded, bcrypt.gensalt(self._rounds)).decode("utf-8")

    def verify(self, plaintext: str, hashed: str) -> bool:
        encoded = plaintext.encode("utf-8")[:MAX_PASSWORD_BYTES]
        try:
            return bcrypt.checkpw(encoded, hashed.encode("utf-8"))
        except ValueError:
            # A stored hash that bcrypt cannot parse is a failed login, not a 500.
            return False

    def verify_decoy(self, plaintext: str) -> bool:
        """Burn the same amount of time as a real verification. Always False."""
        bcrypt.checkpw(plaintext.encode("utf-8")[:MAX_PASSWORD_BYTES], self._decoy_hash)
        return False
