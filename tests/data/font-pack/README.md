# Test font pack

A stand-in for PSCat's `cjk-thai` font pack, laid out as `PSCAT_HOME=<dir>
pscat fonts install` writes it (`fonts/packs/cjk-thai/`, with the
`install.json` PSCat reads to recognise an installed pack). It is not a
real pack: it holds one face, `NotoSansJP/NotoSansJP-Regular.ttf`, cut down
to ASCII and the characters 日本語テストあいう品川 (about 13 KB), so the
tests can draw Japanese text without a 9.5 MB font.

* Source: Noto Sans JP Regular from the `cjk-thai` pack of the pinned PSCat
  (`fonts/catalog/NotoSansJP`), licensed under the SIL Open Font License
  1.1 (`OFL.txt` beside the font). The reserved font name of that licence
  is "Source", which the subset does not use.
* Made with: `pyftsubset NotoSansJP-Regular.ttf --text="日本語テストあいう 品川"
  --unicodes=U+0020-007E --layout-features= --name-IDs='*' --no-hinting
  --notdef-outline --drop-tables+=GSUB,GPOS,GDEF,STAT,HVAR,MVAR,avar,fvar,gvar`
* The tests copy this tree into a scratch directory before using it, so the
  checked-in files are never the directory a server is pointed at.
