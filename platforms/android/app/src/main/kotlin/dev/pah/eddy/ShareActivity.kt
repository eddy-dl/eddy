package dev.pah.eddy

import android.app.Activity
import android.os.Bundle
import android.widget.Toast

// "Add to Eddy" from the share sheet: saves the links as pending, shows a
// toast and closes, without opening the app. The app picks them up from the
// database the next time it comes to the front.
class ShareActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val shared = SharedContent.from(this, intent)
        val added = if (shared != null) {
            EddyNative.init(applicationContext)
            EddyNative.addShared(shared.text, shared.path, shared.name)
        } else {
            0
        }
        val message = if (added > 0) {
            resources.getQuantityString(R.plurals.links_added, added, added)
        } else {
            getString(R.string.no_links_found)
        }
        Toast.makeText(this, message, Toast.LENGTH_SHORT).show()
        finish()
    }
}
