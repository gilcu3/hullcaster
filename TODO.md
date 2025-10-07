# TODO

Tasks that I plan to do. PRs solving any of them are also welcome.

- [x] Syncing with gpodder episode actions API (rudimentary, tested with
  [opodsync](https://github.com/kd2org/opodsync))
- [x] Fix performance of mark-all as read
- [x] Fix bug deleting file
- [x] Show the podcast description only when a podcast is selected
- [x] Fix syncing podcasts, sometimes gets stuck (it seems timeout not working)
- [x] Support downloading subscriptions from gpodder
- [x] Support uploading subscriptions to gpodder
- [x] If gpodder is on, update actions right after syncing podcasts
- [x] Prevent screen flickering while syncing podcasts
- [x] Prevent screen flickering at the end of lists
- [x] Fix vertical screens
- [x] Clear details panel when nothing is selected
- [x] Make queue persistent
- [x] Fix crash when playing from queue
- [x] Create lockfile to prevent several instances of hullcaster
- [x] Add option to play next from queue automatically, makes sense only after internal player is implemented
- [ ] Add history of episode actions. It seems that `AntennaPod` does not do this.
- [x] Avoid repeated elements in queue
- [x] Fix bug where queue actions are not persistent, hard to reproduce
- [ ] Fix gpodder test, it should use local files or local server
- [x] Add panel for unplayed episodes across podcasts
- [x] Fix bug in two-column state, going to/from queue from/to episode/unplayed panel does not work
- [x] Unplayed items not sorted correctly
- [x] Errors appear sporadically synchronizing podcasts with gpodder
- [x] Unplayed items not updated after sync
- [x] `~` folder created when creating logs
- [x] Show in the UI when an episode is currently downloaded
- [ ] Add tests for the new UI
- [x] Add config parameter `confirm_on_exit`.
- [x] Make gpodder synchronization async, cope with bad network conditions
- [x] Support streaming play
- [x] Fix gpodder sync not working when episode link needs redirects
- [ ] Fix initial setup issues when using gpodder
- [ ] Revise threadpool implementation, convert it to async
- [ ] Revise player implementation, make it truly async
- [x] Revise media_control implementation, make it async
- [ ] Revise gpodder implementation, make it truly async
- [ ] Fix seek backwards
- [ ] Add support for changing audio speed
