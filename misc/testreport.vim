" Vim syntax file
" Language: QAM Testreport
" Maintainer: Jan Baier
" Latest Revision: 2 May 2019

if exists("b:current_syntax")
	finish
endif

" Keywords
syn keyword positiveKeyword PASSED SUCCEEDED YES
syn keyword negativeKeyword FAILED NO

" Matches
syn match Comment 'For more details see "regression testing" below.'
syn match Identifier /\s*=>/
syn match Identifier /^[A-Z][A-Za-z_ ]\+:/
syn match Label /^#\+\n.*\n#\+$/
syn match Label /^.\+:\?\n-\+$/
syn match Label /^\(.* \)\?SUMMARY:\?\(\n=\+\)\?/
syn match Macro /\(new \)\?bugs.*:$/
syn match Macro /^\(before\|after\|scripts\):/
syn match QAMComment /comment:.*/
syn match Error "(put your details here)"
syn match Error "Suggested Test Plan Reviewers: .*"
syn match Error /Example.*:/
syn match Error /SUMMARY:\s*PASSED\/FAILED/

" Region
syn region Comment start="put here the output of the following commands:" end="## export MTUI:.*"
syn region Comment start="List of testcases in Testopia:" end="https://bugzilla.*"
syn region Comment start="Put here the assessment" end="report directory."

" Highlights
hi negativeKeyword ctermfg=darkred
hi positiveKeyword ctermfg=darkgreen
hi QAMComment ctermfg=white
