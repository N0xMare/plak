from pathlib import Path


def test_hello_txt():
    path = Path("/app/hello.txt")
    assert path.is_file(), "hello.txt must exist"
    assert path.read_text() == "hello\n"
