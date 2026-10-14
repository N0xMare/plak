import subprocess


def test_greet_py():
    result = subprocess.run(
        ["python3", "/app/greet.py"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, f"expected exit 0, got {result.returncode}: {result.stderr}"
    assert result.stdout == "Hello\n"
