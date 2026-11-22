#!/usr/bin/env python3
"""Check authored expectations for a synthetic fixed source-clock window."""
import json
import re
import sys


class FixtureError(ValueError):
    pass


def obj(value, keys, where):
    if not isinstance(value, dict) or set(value) != set(keys):
        raise FixtureError(f"{where}: expected object with keys {', '.join(keys)}")
    return value


def name(value, where):
    if not isinstance(value, str) or not value.strip():
        raise FixtureError(f"{where}: expected non-empty string")
    return value


def tick(value, where):
    if not isinstance(value, str) or not re.fullmatch(r"0|[1-9][0-9]*", value):
        raise FixtureError(f"{where}: expected unsigned canonical decimal integer string")
    return int(value)


def pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise FixtureError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def verify(data):
    obj(data, ('window', 'observed_end', 'events'), 'fixture')
    window = obj(data['window'], ('clock', 'start', 'deadline'), 'window')
    clock = name(window['clock'], 'window.clock')
    start = tick(window['start'], 'window.start')
    deadline = tick(window['deadline'], 'window.deadline')
    if start >= deadline:
        raise FixtureError('window: start must be less than deadline')
    end = obj(data['observed_end'], ('clock', 'tick', 'order'), 'observed_end')
    name(end['clock'], 'observed_end.clock')
    tick(end['tick'], 'observed_end.tick')
    if type(end['order']) is not int or end['order'] < 0:
        raise FixtureError('observed_end.order: expected nonnegative integer')
    events = data['events']
    if not isinstance(events, list) or not events:
        raise FixtureError('events: expected non-empty array')
    ids, orders, failures = set(), {end['order']}, []
    for index, event in enumerate(events):
        where = f'events[{index}]'
        obj(event, ('id', 'source_clock', 'source_tick', 'observed_clock',
                    'observed_tick', 'observed_order', 'expected'), where)
        ident = name(event['id'], where + '.id')
        if ident in ids:
            raise FixtureError(f'{where}: duplicate event id {ident!r}')
        ids.add(ident)
        source_clock = event['source_clock']
        if source_clock is not None:
            name(source_clock, where + '.source_clock')
        source_tick = tick(event['source_tick'], where + '.source_tick')
        name(event['observed_clock'], where + '.observed_clock')
        tick(event['observed_tick'], where + '.observed_tick')
        order = event['observed_order']
        if type(order) is not int or order < 0 or order in orders:
            raise FixtureError(f'{where}.observed_order: expected unique nonnegative integer')
        orders.add(order)
        expected = event['expected']
        if expected not in ('in_window', 'outside_window', 'unknown_clock'):
            raise FixtureError(f'{where}.expected: invalid label {expected!r}')
        actual = ('unknown_clock' if source_clock != clock else
                  'in_window' if start <= source_tick < deadline else 'outside_window')
        if actual != expected:
            failures.append(f'{ident}: expected {expected}, actual {actual}')
    return len(events), failures


def main():
    if len(sys.argv) != 2:
        print('usage: python verify_window.py FIXTURE.json', file=sys.stderr)
        return 2
    try:
        with open(sys.argv[1], encoding='utf-8') as stream:
            data = json.load(stream, object_pairs_hook=pairs)
        count, failures = verify(data)
    except (OSError, UnicodeError, ValueError, TypeError) as exc:
        print(f'invalid fixture: {exc}', file=sys.stderr)
        return 2
    if failures:
        for failure in failures:
            print(failure, file=sys.stderr)
        print(f'FAIL: {len(failures)} of {count} events differ', file=sys.stderr)
        return 1
    print(f'PASS: {count} event classifications match')
    return 0


if __name__ == '__main__':
    sys.exit(main())
