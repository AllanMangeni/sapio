import functools
from typing import List, Tuple

from bitcoin_script_compiler import (
    Wait,
    SignedBy,
    Weeks,
)
from bitcoinlib.static_types import Amount, PubKey
from sapio_compiler import Contract, TransactionTemplate, guarantee, unlock


def segment_by_radix(L, n):
    size = max(len(L) // n, n)
    for i in range(0, len(L), size):
        if i + size + size > len(L):
            yield L[i:]
            return
        else:
            yield L[i : i + size]


class TreePay(Contract):
    class Fields:
        payments: List[Tuple[Amount, Contract]]
        radix: int

    @guarantee
    def expand(self) -> TransactionTemplate:
        tx = TransactionTemplate()
        segments = list(
            segment_by_radix(self.payments, self.radix)
        )
        if len(segments) == 1:
            for payment in self.payments:
                tx.add_output(payment[0], payment[1])
        else:
            for segment in segments:
                amount = functools.reduce(
                    lambda x, y: x + y, [a for (a, _) in segment], Amount(0)
                )
                tx.add_output(
                    amount, TreePay(payments=segment, radix=self.radix)
                )
        return tx


def libsecp_make_musig(x):
    return "0" * 32


class CollapsibleTree(Contract):
    class Fields:
        payments: List[Tuple[Amount, Contract]]
        radix: int

    @guarantee
    def expand(self) -> TransactionTemplate:
        tx = TransactionTemplate()
        segments = list(
            segment_by_radix(self.payments, self.radix)
        )
        if len(segments) == 1:
            for payment in self.payments:
                tx.add_output(payment[0], payment[1])
        else:
            for segment in segments:
                amount = functools.reduce(
                    lambda x, y: x + y, [a for (a, _) in segment], Amount(0)
                )
                tx.add_output(
                    amount, TreePay(payments=segment, radix=self.radix)
                )
        return tx

    def get_musig(self) -> PubKey:
        return PubKey(b"0" * 32)

    @unlock
    def cooperate_out(self):
        return SignedBy(self.get_musig())