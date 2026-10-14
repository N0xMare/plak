from pathlib import Path


def test_sum_txt():
    path = Path("/app/sum.txt")
    assert path.is_file(), "sum.txt must exist"
    assert path.read_text() == "20\n"
