" Popup menu for selecting a past assistant message to edit.
" Sourced via `vim -S menu.vim <msg_file>`. Reads paths from env vars
" (CVIM_SEEDS_DIR, CVIM_MSG_FILE, CVIM_ORIG_FILE, CVIM_OFFSET_FILE,
" CVIM_MENU_SELECTED_FILE, CVIM_MENU_JSON). Silently finishes on any
" precondition miss; cvim-command pre-seeds offset=0 into msg_file/
" orig_file so the editor falls through to the newest message instead
" of an empty buffer.

if !has('popupwin') || !exists('*json_decode')
  finish
endif

let s:menu_json = $CVIM_MENU_JSON
let s:seeds_dir = $CVIM_SEEDS_DIR
let s:msg_file = $CVIM_MSG_FILE
let s:orig_file = $CVIM_ORIG_FILE
let s:offset_file = $CVIM_OFFSET_FILE
let s:selected_file = $CVIM_MENU_SELECTED_FILE

if empty(s:menu_json) || !filereadable(s:menu_json)
  finish
endif

try
  " cvim-list emits newest-first (offset=0 at index 0). Reverse for display
  " so the list reads chronologically: oldest on top, newest at the bottom.
  " The `offset` field on each entry is absolute (0 = newest), so the pick
  " callback can use natural 1-based indexing against the reversed list.
  let s:menu = reverse(json_decode(join(readfile(s:menu_json), "\n")))
catch
  finish
endtry

if type(s:menu) != type([]) || empty(s:menu)
  finish
endif

" Captured in HiveCvimMenuShow under VimEnter, before the popup steals
" focus. Used by the filter to reload the main buffer when the highlighted
" menu row changes (live preview).
let s:main_winid = 0

" Set to 1 in HiveCvimMenuPick when the user actually picks an entry.
" HiveCvimMenuRestoreOnAbort (VimLeavePre) reads this to decide whether
" the in-progress preview should be rolled back so the post-script's
" ``cmp -s msg_file orig_file`` skips sendback. Covers any exit path
" the popup callback doesn't see (``:q``, ``:qa!``, abrupt pane close).
let s:selection_committed = 0

function! HiveCvimMenuPick(id, result) abort
  if a:result < 1 || a:result > len(s:menu)
    " Cancel (Esc). VimLeavePre rolls msg_file back from orig_file.
    silent quitall!
    return
  endif
  let l:entry = s:menu[a:result - 1]
  let l:seed_file = s:seeds_dir . '/' . l:entry.offset . '.md'
  if !filereadable(l:seed_file)
    silent quitall!
    return
  endif
  let l:content = readfile(l:seed_file)
  call writefile(l:content, s:orig_file)
  call writefile(l:content, s:msg_file)
  call writefile([string(l:entry.offset)], s:offset_file)
  call writefile(['1'], s:selected_file)
  let s:selection_committed = 1
  silent execute 'edit!'
  normal! gg
endfunction

function! HiveCvimMenuRestoreOnAbort() abort
  if s:selection_committed
    return
  endif
  if filereadable(s:orig_file)
    call writefile(readfile(s:orig_file), s:msg_file)
  endif
endfunction

function! HiveCvimSyncPreview(popup_winid) abort
  " Reload the main buffer with the seed of the row currently highlighted
  " in the popup. Intentionally writes msg_file only — orig_file stays at
  " the initial offset=0 seed so a later cancel can restore cleanly.
  let l:line = line('.', a:popup_winid)
  if l:line < 1 || l:line > len(s:menu)
    return
  endif
  let l:entry = s:menu[l:line - 1]
  let l:seed_file = s:seeds_dir . '/' . l:entry.offset . '.md'
  if !filereadable(l:seed_file)
    return
  endif
  call writefile(readfile(l:seed_file), s:msg_file)
  if s:main_winid > 0
    call win_execute(s:main_winid, 'silent! edit!')
    call win_execute(s:main_winid, 'silent! normal! gg')
  endif
endfunction

function! HiveCvimMenuFilter(winid, key) abort
  let l:handled = popup_filter_menu(a:winid, a:key)
  if l:handled
    call HiveCvimSyncPreview(a:winid)
  endif
  return l:handled
endfunction

function! HiveCvimMenuShow() abort
  let s:main_winid = win_getid()
  let l:labels = map(copy(s:menu), 'v:val.label')
  let l:winid = popup_menu(l:labels, {
        \ 'title': ' assistant messages (↑↓ · Enter select · Esc cancel) ',
        \ 'callback': 'HiveCvimMenuPick',
        \ 'filter': 'HiveCvimMenuFilter',
        \ 'border': [1, 1, 1, 1],
        \ 'padding': [0, 1, 0, 1],
        \ 'maxheight': 15,
        \ 'maxwidth': 100,
        \ })
  " Land cursor on the newest entry (last row) so Enter selects it by default.
  if l:winid > 0
    call win_execute(l:winid, 'normal! G')
    " Initial preview = highlighted row (newest message).
    call HiveCvimSyncPreview(l:winid)
  endif
endfunction

augroup HiveCvimMenu
  autocmd!
  autocmd VimEnter * call HiveCvimMenuShow()
  autocmd VimLeavePre * call HiveCvimMenuRestoreOnAbort()
augroup END
