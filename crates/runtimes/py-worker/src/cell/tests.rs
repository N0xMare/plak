use super::without_prelude_frames;

#[test]
fn only_exact_prelude_frame_blocks_are_removed() {
    let head = concat!(
        "Traceback (most recent call last):\n",
        "  File \"<cell 6>\", line 2, in <module>\n",
        "    await run('λ')\n",
        "    ^~~~~~~~~~~~~~\n",
    );
    let tail = concat!(
        "  File \"test-plak-python-prelude.py\", line 4, in ordinary\n",
        "    fail()\n",
        "ToolError: unchanged. Did you mean: something else?\n",
        "  File \"plak-python-prelude.py\", line 5, in note\n",
        "    not part of a traceback\n",
        "\nThe above exception was the direct cause of the following exception:\n\n",
        "  + Exception Group Traceback (most recent call last):\n",
        "  |   File \"<cell 6>\", line 7, in group\n",
        "  |     raise group\n",
    );
    let prelude = concat!(
        "  File \"plak-python-prelude.py\", line 3, in run\n",
        "    return await _run()\n",
        "           ^^^^^^^^^^^\n",
    );
    let group_prelude = concat!(
        "  |   File \"plak-python-prelude.py\", line 8, in grouped\n",
        "  |     return await _grouped()\n",
    );
    let end = "  | ExceptionGroup: failed (1 sub-exception)\nunchanged final note";
    let report = format!("{head}{prelude}{tail}{group_prelude}{end}");
    assert_eq!(
        without_prelude_frames(&report),
        format!("{head}{tail}{end}")
    );
}
