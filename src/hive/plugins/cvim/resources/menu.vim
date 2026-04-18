" Popup menu for selecting a past assistant message to edit.
" Sourced via `vim -S menu.vim <msg_file>`. Reads paths from env vars
" (CVIM_SEEDS_DIR, CVIM_MSG_FILE, CVIM_ORIG_FILE, CVIM_OFFSET_FILE,
" CVIM_MENU_SELECTED_FILE, CVIM_MENU_JSON). Silently finishes on any
" precondition miss so the editor falls through to a blank buffer.

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

function! HiveCvimMenuPick(id, result) abort
  if a:result < 1 || a:result > len(s:menu)
    " User cancelled (Esc). Exit vim entirely so /cvim dismisses in one
    " keystroke rather than leaving the user in an empty buffer.
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
  silent execute 'edit!'
  normal! gg
endfunction

function! HiveCvimMenuShow() abort
  let l:labels = map(copy(s:menu), 'v:val.label')
  let l:winid = popup_menu(l:labels, {
        \ 'title': ' assistant messages (↑↓ · Enter select · Esc cancel) ',
        \ 'callback': 'HiveCvimMenuPick',
        \ 'border': [1, 1, 1, 1],
        \ 'padding': [0, 1, 0, 1],
        \ 'maxheight': 15,
        \ 'maxwidth': 100,
        \ })
  " Land cursor on the newest entry (last row) so Enter selects it by default.
  if l:winid > 0
    call win_execute(l:winid, 'normal! G')
  endif
endfunction

augroup HiveCvimMenu
  autocmd!
  autocmd VimEnter * call HiveCvimMenuShow()
augroup END
