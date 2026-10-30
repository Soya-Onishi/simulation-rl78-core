from systemtest import Duration


def test_duration_units() -> None:
    assert Duration.ns(10).nanoseconds == 10
    assert Duration.us(2).nanoseconds == 2_000
    assert Duration.ms(3).nanoseconds == 3_000_000
    assert Duration.sec(1).nanoseconds == 1_000_000_000
    assert Duration.sec(0.001).nanoseconds == 1_000_000
