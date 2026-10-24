# Vendored verbatim from the competition metric notebook,
# https://www.kaggle.com/code/metric/squares-in-squares (pulled 2026-10-23 with
# `kernels/pull`). Our scorer must agree with Kaggle's, so it IS Kaggle's: never
# edit this file; re-pull it if the notebook changes.
"""Metric for the "Packing N Unit Squares into the Smallest Square" competition.

Participants submit packings of n unit squares (n=1..1000) inside a bounding
square.  Each square is specified by its center (x, y) and rotation angle.
Coordinates must be plain decimal literals, optionally in scientific notation
(e.g. ``2.70710678118654752440`` or ``1.5e-3``).  Symbolic / closed-form
expressions are not accepted -- submit their decimal expansion instead, at as
many digits as you like.

Scoring
-------
For each puzzle n the bounding side length L is measured
translation-invariantly as ``max(max_x - min_x, max_y - min_y)`` over all
corners.  The per-puzzle score is the excess over the area lower bound

    score(n) = L / sqrt(n) - 1

which is 0.0 exactly when the packing achieves the area lower bound (only
possible for perfect squares packed on the grid) and grows above 0 as the
bounding square wastes area.  **Lower is better**; total score is the sum
across n=1..N.

+-------------------+---------------------------------------------------------+
| Condition         | Score                                                   |
+===================+=========================================================+
| feasible packing  | L / sqrt(n) - 1     (>= 0 always)                       |
+-------------------+---------------------------------------------------------+
| infeasible/invalid| L_0 / sqrt(n) - 1   (grid ceiling — no worse than       |
|                   |                      submitting the trivial grid)       |
+-------------------+---------------------------------------------------------+

For reference: ``L_B = sqrt(n)`` is the area lower bound and
``L_0 = ceil(sqrt(n))`` is the trivial grid packing.  The best-known side
length plays no part -- beating a record just yields a smaller excess -- so
this metric reads no baseline values at all.

Feasibility is checked with interval arithmetic (mpmath) at escalating
precision to certify that no two squares overlap.

Kaggle entry point
------------------
``score(solution, submission, row_id_column_name)`` — called by the Kaggle
backend.  ``solution`` is a DataFrame loaded from ``solution.csv``; only its
``n`` column is read, to fix which puzzles are scored (and which absent ones
are charged the grid ceiling).  ``submission`` is the participant CSV with
columns ``n, square_id, x, y, angle``.

As a side effect it writes ``submission_details.csv``, one row per puzzle with
columns ``n`` and ``L``::

    n,L
    1,1.00000000000000000000000000000000000000000000000000
    2,2.00000000000000000000000000000000000000000000000000
    ...

``L`` is the measured bounding side for puzzle n -- not the per-puzzle score --
written with exactly 50 digits after the decimal point.  Those digits are
significant: L is measured in ``L_DPS``-digit mpmath arithmetic straight from
the submitted decimal strings, not from a double.  Puzzles with no accepted L
(absent from the submission, unparseable, failing column validation, or
rejected as infeasible) are written as zero.
"""

import contextlib
import csv
import io
import math
import multiprocessing
import re
from collections import defaultdict
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from fractions import Fraction
from pathlib import Path

import mpmath
import pandas as pd

# ---------------------------------------------------------------------------
# interval
# ---------------------------------------------------------------------------

IVal = mpmath.ctx_iv.ivmpf

# Working precision for the (non-interval) measurement of the bounding side L,
# and the number of digits after the decimal point written for it in
# submission_details.csv.  The margin between the two keeps every written digit
# correct: the only rounding is in the trig of the corner computation, tens of
# digits below the last digit reported.
L_DPS = 60
L_DECIMALS = 50


@contextlib.contextmanager
def working_dps(dps: int):
    """Run a block with both mpmath contexts at ``dps`` digits, then restore them.

    ``mpmath.iv`` carries its own precision, independent of ``mpmath.mp``:
    setting only ``mpmath.mp.dps`` leaves every interval operation at the
    default 53 bits, which caps what a contact can be certified to at ~1e-13 no
    matter how far the escalation sequence climbs.  Both contexts are set here
    because the interval work is interleaved with plain ``mp`` arithmetic.

    >>> with working_dps(500):
    ...     width = mpmath.mpf(mpmath.iv.mpf('0.1').delta)
    >>> width < mpmath.mpf(10) ** -500
    True
    """
    old_iv_dps = mpmath.iv.dps
    old_mp_dps = mpmath.mp.dps
    try:
        mpmath.iv.dps = dps
        mpmath.mp.dps = dps
        yield
    finally:
        mpmath.iv.dps = old_iv_dps
        mpmath.mp.dps = old_mp_dps


def make_interval(value: str, dps: int = 50) -> IVal:
    with working_dps(dps):
        return mpmath.iv.mpf(value)


def iv_cos(x: IVal) -> IVal:
    return mpmath.iv.cos(x)


def iv_sin(x: IVal) -> IVal:
    return mpmath.iv.sin(x)


_ESCALATION_SEQUENCE = [50, 100, 200, 500]


def escalate(
    fn: Callable[[int], bool | None],
    max_dps: int = 500,
    start_dps: int = 50,
) -> bool | None:
    for dps in _ESCALATION_SEQUENCE:
        if dps < start_dps:
            continue
        if dps > max_dps:
            break
        result = fn(dps)
        if result is not None:
            return result
    return None


def iv_le(a: IVal, b: IVal) -> bool | None:
    """Certified less-or-equal: True if a <= b, False if a > b, None if undecided.

    Comparing two overlapping intervals is indeterminate.  mpmath 1.3 reports
    that as None, 1.4 raises ValueError; both mean "undecided" here.
    """
    try:
        return a <= b
    except ValueError:
        return None


def iv_min(values: Sequence[IVal]) -> IVal:
    """Interval extension of ``min``: the tightest interval containing the true minimum.

    Comparison operators are useless for this -- two overlapping intervals have
    no determinate order -- so take it endpoint-wise instead.  min of the lower
    bounds is a lower bound on the true min, and min of the upper bounds is an
    upper bound on it, whichever element turns out to be smallest.
    """
    return mpmath.iv.mpf([min(v.a for v in values), min(v.b for v in values)])


def iv_max(values: Sequence[IVal]) -> IVal:
    """Interval extension of ``max``. See :func:`iv_min`."""
    return mpmath.iv.mpf([max(v.a for v in values), max(v.b for v in values)])


# ---------------------------------------------------------------------------
# parser
# ---------------------------------------------------------------------------

# Bounds the cost of parsing one coordinate.  Far above any useful precision
# (L is reported to 50 decimals), so no honest submission comes near it.
_MAX_LITERAL_CHARS = 1000

_DECIMAL_RE = re.compile(r'^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$')


def _is_decimal(s: str) -> bool:
    return len(s) <= _MAX_LITERAL_CHARS and bool(_DECIMAL_RE.match(s.strip()))


def _check_decimal(s: str) -> str:
    """Return the stripped literal, or raise if it is not a plain decimal.

    >>> _check_decimal(" 2.70710678118654752440 ")
    '2.70710678118654752440'
    >>> _check_decimal("1.5e-3")
    '1.5e-3'
    >>> _check_decimal("1 + sqrt(2)")
    Traceback (most recent call last):
        ...
    ValueError: not a decimal number: '1 + sqrt(2)'
    """
    s = s.strip()
    if not _is_decimal(s):
        shown = s if len(s) <= 40 else s[:40] + '...'
        raise ValueError(f'not a decimal number: {shown!r}')
    return s


def parse_value(s: str, dps: int = 50) -> IVal:
    """Parse a decimal literal into an enclosing interval at ``dps`` digits."""
    return make_interval(_check_decimal(s), dps)


def parse_value_mp(s: str, dps: int = L_DPS) -> mpmath.mpf:
    """Parse a decimal literal into an ``mpf`` at ``dps`` digits.

    >>> mpmath.nstr(parse_value_mp("2.70710678118654752440"), 21, strip_zeros=False)
    '2.70710678118654752440'
    """
    s = _check_decimal(s)
    old_dps = mpmath.mp.dps
    try:
        mpmath.mp.dps = dps
        return mpmath.mpf(s)
    finally:
        mpmath.mp.dps = old_dps


def parse_value_float(s: str) -> float:
    """Parse a decimal literal to float.

    >>> parse_value_float("3.5")
    3.5
    >>> parse_value_float("-2.5")
    -2.5
    >>> parse_value_float("1.5e-3")
    0.0015

    Values too large for a double are rejected rather than becoming ``inf``,
    which would poison every downstream geometry check:

    >>> parse_value_float("1e400")
    Traceback (most recent call last):
        ...
    ValueError: value out of range for a double: '1e400'
    """
    s = _check_decimal(s)
    value = float(s)
    if not math.isfinite(value):
        raise ValueError(f'value out of range for a double: {s!r}')
    return value


# ---------------------------------------------------------------------------
# scoring
# ---------------------------------------------------------------------------


def score_puzzle(n: int, s_certified: float) -> float:
    """Score a single puzzle: L / sqrt(n) - 1 (lower is better).

    Result is >= 0 for any feasible packing (equality only for perfect-square
    grid packings).  The best-known side length s* plays no part: beating a
    record simply yields a smaller excess.

    Perfect-square grid packing scores 0 exactly:
    >>> score_puzzle(4, 2.0)
    0.0

    Larger L → larger (worse) score:
    >>> round(score_puzzle(5, 2.707), 6)
    0.210607

    Doubling L takes the score from 0 to 1:
    >>> score_puzzle(4, 4.0)
    1.0
    """
    L_B = math.sqrt(n)
    return s_certified / L_B - 1.0


def infeasible_score(n: int) -> float:
    """Penalty score for an infeasible / invalid submission for puzzle n.

    Equal to submitting the trivial grid packing: L_0 / sqrt(n) - 1.
    """
    return math.ceil(math.sqrt(n)) / math.sqrt(n) - 1.0


# ---------------------------------------------------------------------------
# geometry
# ---------------------------------------------------------------------------


@dataclass
class Square:
    x: float
    y: float
    angle: float
    x_str: str = ''
    y_str: str = ''
    angle_str: str = ''
    _corners_float: list[tuple[float, float]] = field(default_factory=list, repr=False)

    def corners_float(self) -> list[tuple[float, float]]:
        if not self._corners_float:
            self._corners_float = compute_corners_float(self.x, self.y, self.angle)
        return self._corners_float


def compute_corners_float(cx: float, cy: float, angle: float) -> list[tuple[float, float]]:
    """Return the four corners of a unit square centered at (cx, cy) rotated by angle.

    >>> corners = compute_corners_float(0.5, 0.5, 0.0)
    >>> [(round(x, 1), round(y, 1)) for x, y in corners]
    [(1.0, 1.0), (0.0, 1.0), (0.0, 0.0), (1.0, 0.0)]
    """
    c = math.cos(angle)
    s = math.sin(angle)
    h = 0.5
    dx1, dy1 = h * c - h * s, h * s + h * c
    dx2, dy2 = -h * c - h * s, -h * s + h * c
    return [
        (cx + dx1, cy + dy1),
        (cx + dx2, cy + dy2),
        (cx - dx1, cy - dy1),
        (cx - dx2, cy - dy2),
    ]


def compute_corners_mp(cx: mpmath.mpf, cy: mpmath.mpf, angle: mpmath.mpf) -> list[tuple[mpmath.mpf, mpmath.mpf]]:
    """``compute_corners_float`` at the ambient ``mpmath.mp`` precision."""
    c = mpmath.cos(angle)
    s = mpmath.sin(angle)
    h = mpmath.mpf(1) / 2
    dx1, dy1 = h * c - h * s, h * s + h * c
    dx2, dy2 = -h * c - h * s, -h * s + h * c
    return [
        (cx + dx1, cy + dy1),
        (cx + dx2, cy + dy2),
        (cx - dx1, cy - dy1),
        (cx - dx2, cy - dy2),
    ]


def measure_side_mp(squares: list['Square'], dps: int = L_DPS) -> mpmath.mpf:
    """Bounding side ``max(max_x - min_x, max_y - min_y)`` at ``dps`` digits.

    Measured from the submitted decimal strings, so a packing whose coordinates
    carry more than double precision keeps that precision in the reported L.

    >>> unit = Square(0.5, 0.5, 0.0, x_str="0.5", y_str="0.5", angle_str="0")
    >>> measure_side_mp([unit])
    mpf('1.0')
    """
    old_dps = mpmath.mp.dps
    try:
        mpmath.mp.dps = dps
        xs: list[mpmath.mpf] = []
        ys: list[mpmath.mpf] = []
        for sq in squares:
            corners = compute_corners_mp(
                parse_value_mp(sq.x_str, dps),
                parse_value_mp(sq.y_str, dps),
                parse_value_mp(sq.angle_str, dps),
            )
            for x, y in corners:
                xs.append(x)
                ys.append(y)
        return max(max(xs) - min(xs), max(ys) - min(ys))
    finally:
        mpmath.mp.dps = old_dps


def compute_corners_interval(cx_iv: IVal, cy_iv: IVal, angle_iv: IVal) -> list[tuple[IVal, IVal]]:
    c = iv_cos(angle_iv)
    s = iv_sin(angle_iv)
    h = mpmath.iv.mpf('0.5')
    dx1, dy1 = h * c - h * s, h * s + h * c
    dx2, dy2 = -h * c - h * s, -h * s + h * c
    return [
        (cx_iv + dx1, cy_iv + dy1),
        (cx_iv + dx2, cy_iv + dy2),
        (cx_iv - dx1, cy_iv - dy1),
        (cx_iv - dx2, cy_iv - dy2),
    ]


def compute_aabb(corners: list[tuple[float, float]]) -> tuple[float, float, float, float]:
    xs = [c[0] for c in corners]
    ys = [c[1] for c in corners]
    return (min(xs), min(ys), max(xs), max(ys))


def aabb_overlap(
    a: tuple[float, float, float, float],
    b: tuple[float, float, float, float],
) -> bool:
    return not (a[2] < b[0] or b[2] < a[0] or a[3] < b[1] or b[3] < a[1])


def _project_corners_float(corners: list[tuple[float, float]], ax: float, ay: float) -> tuple[float, float]:
    projs = [ax * cx + ay * cy for cx, cy in corners]
    return (min(projs), max(projs))


def sat_check_float(sq1: Square, sq2: Square, undecided_tol: float = 1e-8) -> str:
    """Check overlap between two unit squares using the Separating Axis Theorem.

    Returns ``"separated"``, ``"overlapping"``, or ``"undecided"``.

    >>> sat_check_float(Square(0.5, 0.5, 0.0), Square(5.0, 5.0, 0.0))
    'separated'
    >>> sat_check_float(Square(0.5, 0.5, 0.0), Square(0.6, 0.6, 0.0))
    'overlapping'
    """
    c1 = sq1.corners_float()
    c2 = sq2.corners_float()

    axes = _get_axes_float(sq1.angle, sq2.angle)

    min_overlap = float('inf')
    for ax, ay in axes:
        min1, max1 = _project_corners_float(c1, ax, ay)
        min2, max2 = _project_corners_float(c2, ax, ay)
        overlap = min(max1, max2) - max(min1, min2)
        if overlap < -undecided_tol:
            return 'separated'
        min_overlap = min(min_overlap, overlap)

    if min_overlap > undecided_tol:
        return 'overlapping'
    return 'undecided'


def _get_axes_float(angle1: float, angle2: float) -> list[tuple[float, float]]:
    c1, s1 = math.cos(angle1), math.sin(angle1)
    c2, s2 = math.cos(angle2), math.sin(angle2)
    return [(c1, s1), (-s1, c1), (c2, s2), (-s2, c2)]


def sat_check_interval(
    sq1_corners: list[tuple[IVal, IVal]],
    sq2_corners: list[tuple[IVal, IVal]],
    angle1_iv: IVal,
    angle2_iv: IVal,
) -> bool | None:
    c1_iv, s1_iv = iv_cos(angle1_iv), iv_sin(angle1_iv)
    c2_iv, s2_iv = iv_cos(angle2_iv), iv_sin(angle2_iv)
    axes = [(c1_iv, s1_iv), (-s1_iv, c1_iv), (c2_iv, s2_iv), (-s2_iv, c2_iv)]

    has_undecided = False
    for ax, ay in axes:
        projs1 = [ax * cx + ay * cy for cx, cy in sq1_corners]
        projs2 = [ax * cx + ay * cy for cx, cy in sq2_corners]

        # Endpoint-wise, via iv_min/iv_max: ordering two overlapping intervals
        # with < is indeterminate, and skipping the projections that compare
        # indeterminately would narrow the span -- which shrinks the overlap and
        # biases the test towards wrongly certifying a pair as separated.
        min1, max1 = iv_min(projs1), iv_max(projs1)
        min2, max2 = iv_min(projs2), iv_max(projs2)

        overlap = iv_min([max1, max2]) - iv_max([min1, min2])
        le_result = iv_le(overlap, mpmath.iv.mpf('0'))
        if le_result is True:
            return True
        if le_result is None:
            has_undecided = True

    if has_undecided:
        return None
    return False


def sort_and_sweep(squares: list[Square]) -> list[tuple[int, int]]:
    n = len(squares)
    if n <= 1:
        return []

    aabbs = []
    for i, sq in enumerate(squares):
        corners = sq.corners_float()
        aabb = compute_aabb(corners)
        aabbs.append((i, aabb))

    aabbs.sort(key=lambda x: x[1][0])

    pairs = []
    for i_idx in range(n):
        i, aabb_i = aabbs[i_idx]
        for j_idx in range(i_idx + 1, n):
            j, aabb_j = aabbs[j_idx]
            if aabb_j[0] > aabb_i[2]:
                break
            if aabb_overlap(aabb_i, aabb_j):
                pairs.append((i, j) if i < j else (j, i))

    return pairs


def verify_puzzle(
    squares: list[Square],
) -> tuple[bool, str]:
    """Check that no two squares overlap, using float SAT then interval arithmetic.

    >>> s1 = Square(0.5, 0.5, 0.0, x_str="0.5", y_str="0.5", angle_str="0")
    >>> s2 = Square(1.5, 0.5, 0.0, x_str="1.5", y_str="0.5", angle_str="0")
    >>> verify_puzzle([s1, s2])
    (True, 'feasible')
    >>> verify_puzzle([Square(0.5, 0.5, 0.0), Square(0.6, 0.6, 0.0)])
    (False, 'Squares 0 and 1 overlap (float check)')
    """
    pairs = sort_and_sweep(squares)
    undecided_pairs = []
    for i, j in pairs:
        result = sat_check_float(squares[i], squares[j])
        if result == 'overlapping':
            return (False, f'Squares {i} and {j} overlap (float check)')
        if result == 'undecided':
            undecided_pairs.append((i, j))

    if undecided_pairs:

        def check_overlap_certified(dps: int) -> bool | None:
            # The whole pair loop runs inside the precision context: parsing at dps
            # digits is not enough, since every later interval operation widens its
            # result to whatever precision mpmath.iv is set to at the time.
            with working_dps(dps):
                still_undecided = []
                for i, j in undecided_pairs:
                    sq_i, sq_j = squares[i], squares[j]
                    cx_i = parse_value(sq_i.x_str, dps)
                    cy_i = parse_value(sq_i.y_str, dps)
                    ai = parse_value(sq_i.angle_str, dps)
                    cx_j = parse_value(sq_j.x_str, dps)
                    cy_j = parse_value(sq_j.y_str, dps)
                    aj = parse_value(sq_j.angle_str, dps)
                    corners_i = compute_corners_interval(cx_i, cy_i, ai)
                    corners_j = compute_corners_interval(cx_j, cy_j, aj)
                    result = sat_check_interval(corners_i, corners_j, ai, aj)
                    if result is False:
                        return False
                    if result is None:
                        still_undecided.append((i, j))
                if still_undecided:
                    return None
                return True

        result = escalate(check_overlap_certified)
        if result is False:
            return (False, 'Squares overlap (certified)')
        if result is None:
            return (False, 'Overlap undecidable at max precision')

    return (True, 'feasible')


# ---------------------------------------------------------------------------
# main / Kaggle entry point
# ---------------------------------------------------------------------------


class ParticipantVisibleError(Exception):
    pass


MAX_N = 1000
KAGGLE_CORES = 4
MIN_PUZZLES_FOR_MP = 4


def _parse_submission_csv(text: str) -> dict[int, list[dict]]:
    reader = csv.DictReader(io.StringIO(text))
    required_cols = {'n', 'square_id', 'x', 'y', 'angle'}
    if not required_cols.issubset(set(reader.fieldnames or [])):
        missing = required_cols - set(reader.fieldnames or [])
        raise ParticipantVisibleError(f'Missing columns: {missing}')

    puzzles: dict[int, list[dict]] = defaultdict(list)
    for row_num, row in enumerate(reader, start=2):
        try:
            n = int(row['n'])
        except (ValueError, TypeError):
            raise ParticipantVisibleError(f'Row {row_num}: invalid n value: {row["n"]!r}')
        if n < 1 or n > MAX_N:
            raise ParticipantVisibleError(f'Row {row_num}: n={n} out of range [1, {MAX_N}]')
        puzzles[n].append(row)

    return dict(puzzles)


def _validate_puzzle(n: int, rows: list[dict]) -> str | None:
    if len(rows) != n:
        return f'n={n}: expected {n} rows, got {len(rows)}'

    square_ids = set()
    for row in rows:
        try:
            sid = int(row['square_id'])
        except (ValueError, TypeError):
            return f'n={n}: invalid square_id: {row["square_id"]!r}'
        square_ids.add(sid)

    expected_ids = set(range(n))
    if square_ids != expected_ids:
        return f'n={n}: square_ids should be 0..{n - 1}, got {sorted(square_ids)}'

    return None


def _build_squares(n: int, rows: list[dict]) -> tuple[list[Square], mpmath.mpf]:
    rows_by_id = {int(r['square_id']): r for r in rows}

    squares = []
    for sid in range(n):
        row = rows_by_id[sid]
        x_str = row['x'].strip()
        y_str = row['y'].strip()
        angle_str = row['angle'].strip()

        try:
            x_f = parse_value_float(x_str)
            y_f = parse_value_float(y_str)
            angle_f = parse_value_float(angle_str)
        except ValueError as e:
            raise ParticipantVisibleError(f'n={n}, square {sid}: cannot parse coordinates: {e}')

        squares.append(
            Square(
                x=x_f,
                y=y_f,
                angle=angle_f,
                x_str=x_str,
                y_str=y_str,
                angle_str=angle_str,
            )
        )

    return squares, measure_side_mp(squares)


def evaluate_single_puzzle(n: int, rows: list[dict]) -> tuple[int, float, str | None, str]:
    """Evaluate one puzzle, returning ``(n, score, L, reason)``.

    ``L`` is decimal text with ``L_DECIMALS`` digits after the point, and is
    reported only for a packing that passed the feasibility check: an
    unparseable, malformed, or overlapping puzzle gets None, so no L is
    published for a packing the metric rejected.  Text rather than a float
    because a double cannot hold those digits; the score itself is a double,
    taken from the same high-precision measurement.
    """
    error = _validate_puzzle(n, rows)
    if error:
        return (n, infeasible_score(n), None, error)

    try:
        squares, s_mp = _build_squares(n, rows)
    except ParticipantVisibleError as e:
        return (n, infeasible_score(n), None, str(e))

    feasible, reason = verify_puzzle(squares)
    if not feasible:
        return (n, infeasible_score(n), None, f'infeasible: {reason}')

    puzzle_score = score_puzzle(n, float(s_mp))
    return (n, puzzle_score, _format_detail(s_mp), 'ok')


def _evaluate_puzzle_worker(args: tuple) -> tuple[int, float, str | None, str]:
    n, rows = args
    try:
        return evaluate_single_puzzle(n, rows)
    except Exception as e:  # noqa: BLE001 - one bad puzzle must not kill the pool
        return (n, infeasible_score(n), None, f'worker error: {e}')


def _evaluate_puzzles_parallel(
    puzzles: dict[int, list[dict]],
    puzzle_ns: set[int],
) -> tuple[float, list[tuple[int, float, str | None, str]]]:
    """Score every n in ``puzzle_ns``; rows for any other n are ignored."""
    work = [(n, rows) for n, rows in sorted(puzzles.items()) if n in puzzle_ns]

    if not work:
        results: list[tuple[int, float, str | None, str]] = []
    elif len(work) < MIN_PUZZLES_FOR_MP:
        results = [evaluate_single_puzzle(n, rows) for n, rows in work]
    else:
        with multiprocessing.Pool(processes=KAGGLE_CORES) as pool:
            results = pool.map(_evaluate_puzzle_worker, work)

    # Any scored n missing from the submission gets the grid-ceiling penalty
    # (lower-is-better metric — otherwise omitting a puzzle would improve the
    # total).
    seen = {n for n, _, _, _ in results}
    for n in puzzle_ns:
        if n not in seen:
            results.append((n, infeasible_score(n), None, 'missing from submission'))
    results.sort(key=lambda r: r[0])

    total = sum(puzzle_score for _, puzzle_score, _, _ in results)
    return total, results


def _format_detail(value: mpmath.mpf) -> str:
    """Decimal text for one per-puzzle L: exactly ``L_DECIMALS`` digits after the point.

    An ``mpf`` is a binary fraction (``man * 2**exp``), so it is converted
    exactly to a ``Fraction`` and rounded once, half-to-even, at the last
    written digit.  ``mpmath.nstr`` counts significant digits rather than
    decimal places, and ``format(mpf, '.50f')`` needs mpmath >= 1.4, so neither
    is usable here.

    >>> _format_detail(mpmath.mpf(2))[:12]
    '2.0000000000'
    >>> len(_format_detail(mpmath.mpf(2)).split('.')[1])
    50
    >>> _format_detail(parse_value_mp('1.41421356237309504880168872420969807856967187537695'))
    '1.41421356237309504880168872420969807856967187537695'
    """
    exact = Fraction(value.man) * Fraction(2) ** value.exp
    scale = 10**L_DECIMALS
    units = round(exact * scale)
    sign = '-' if units < 0 else ''
    whole, frac = divmod(abs(units), scale)
    return f'{sign}{whole}.{frac:0{L_DECIMALS}d}'


def score(
    solution: pd.DataFrame,
    submission: pd.DataFrame,
    row_id_column_name: str,
) -> float:
    """Kaggle entry point. Score a submission against the solution's puzzle set.

    ``solution`` is read for one thing only: the set of n to score.  Everything
    else about a puzzle -- best-known side length, proven-optimal flag, source
    -- is irrelevant to ``L / sqrt(n) - 1``.

    >>> import pandas as pd
    >>> sol = pd.DataFrame({"id": [0], "n": [1]})
    >>> sub = pd.DataFrame({"id": [0], "n": [1], "square_id": [0],
    ...     "x": ["0.5"], "y": ["0.5"], "angle": ["0"]})
    >>> score(sol, sub, "id")  # single unit square, L=1, L/sqrt(1) - 1 = 0.0
    0.0
    """
    solution = solution.drop(columns=[row_id_column_name])
    submission = submission.drop(columns=[row_id_column_name])

    puzzle_ns = {int(n) for n in solution['n']}

    text = submission.to_csv(index=False)
    puzzles = _parse_submission_csv(text)
    total, results = _evaluate_puzzles_parallel(puzzles, puzzle_ns)

    # L is the measured bounding side for puzzle n -- NOT the per-puzzle score
    # L/sqrt(n) - 1.  Puzzles with no accepted L (missing from the submission,
    # unparseable, failing column validation, or rejected as infeasible) keep
    # the historical 0 placeholder: an L measured off an overlapping packing is
    # not a real packing of n squares, and downstream harvesting treats every
    # published L as attainable.  Values arrive here already rendered as decimal
    # text with L_DECIMALS digits after the point; unlike a JSON float they are
    # not re-serialized downstream, so all 50 digits reach the stored file
    # verbatim.
    result_map = {n: L_text for n, _, L_text, _ in results if L_text is not None}
    zero = _format_detail(mpmath.mpf(0))

    output_dir = Path('/kaggle/working')
    if not output_dir.is_dir():
        output_dir = Path('.')
    with open(output_dir / 'submission_details.csv', 'w', newline='') as f:
        writer = csv.writer(f)
        writer.writerow(['n', 'L'])
        for n in range(1, MAX_N + 1):
            writer.writerow([n, result_map.get(n, zero)])

    return float(total)
