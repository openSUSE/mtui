augroup filetypedetect
	au BufNewFile,BufRead log if getline(1) =~ '^SUMMARY:' | setf testreport | endif
augroup END
