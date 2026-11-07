"""Regenerate presign.txt with the reference CLI's botocore (offline):

    python tests/fixtures/s3/generate_presign.py > tests/fixtures/s3/presign.txt
"""

import datetime
import sys
from unittest import mock

import awscli.botocore.session as session

sess = session.get_session()
sess.set_credentials("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY")
client = sess.create_client("s3", region_name="us-east-1")
# The signer's own auth module (botocore may be importable under two names).
signer = client._request_signer
auth = sys.modules[type(signer).__module__.rsplit(".", 1)[0] + ".auth"]
with mock.patch.object(auth, "datetime") as dt:
    dt.datetime.utcnow.return_value = datetime.datetime(2015, 8, 30, 12, 36, 0)
    print(
        client.generate_presigned_url(
            "get_object", {"Bucket": "bkt", "Key": "dir/my key.txt"}, ExpiresIn=600
        )
    )
