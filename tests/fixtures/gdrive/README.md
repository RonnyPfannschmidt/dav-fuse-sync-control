# Google Drive fixtures

**These are synthetic.** They are written from the documented Drive v3 response
schemas, not captured from a live account, because there is no Drive emulator
and obtaining a usable credential is blocked on issue #1.

They are still worth having: they pin the JSON→`RemoteEntry` mapping, which is
where a hand-written client goes wrong, and they make the failure modes
(`403 userRateLimitExceeded` vs `403 insufficientFilePermissions`, an aged-out
page token) reproducible.

**Replace them with real captures as soon as a credential exists.** The shape to
capture, with an access token in `$TOKEN`:

```bash
B=https://www.googleapis.com/drive/v3
F='id,name,mimeType,parents,size,modifiedTime,createdTime,md5Checksum,version,trashed,headRevisionId,shortcutDetails(targetId,targetMimeType)'

curl -sH "Authorization: Bearer $TOKEN" \
  "$B/files?q=%27root%27+in+parents+and+trashed+%3D+false&fields=nextPageToken,incompleteSearch,files($F)&pageSize=1000" \
  > files_list_root.json

curl -sH "Authorization: Bearer $TOKEN" "$B/changes/startPageToken" > start_page_token.json
curl -sH "Authorization: Bearer $TOKEN" \
  "$B/changes?pageToken=1&includeRemoved=true&fields=nextPageToken,newStartPageToken,changes(fileId,removed,changeType,file($F))" \
  > changes_page.json
```

Scrub real file names and ids before committing.
